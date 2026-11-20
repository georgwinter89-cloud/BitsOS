//! GP-CPU item 4: the reboot and power-off plans on the FADT shape an Intel desktop board hands over, in a whole
//! table set with the Gaming-PC's MADT and DMAR. What is known of the real FADT is its size (276 bytes, revision 6)
//! and the PM timer port 0x1808 (Linux's journal); every other value here is what such boards usually have and is
//! unverified until the first boot prints the `acpi fadt` line. The test proves the plans follow the tables, whatever
//! they say: a reset register at 0xCF9, PM1 blocks at 0x1800, a 24-bit PM timer, `_S5` type 7.

use crate::fadt::{boot_arch, flags};
use crate::parse::parse;
use crate::power::{poweroff_plan, reboot_plan, Step};
use crate::sdt::Space;
use crate::testkit::*;
use alloc::vec::Vec;

const DMAR_AT: u64 = at::BASE + 0xE000;

fn gas(space: u8, width: u8, access: u8, addr: u64) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = space;
    g[1] = width;
    g[3] = access;
    g[4..].copy_from_slice(&addr.to_le_bytes());
    g
}

/// The Mini-PC's FADT with the fields an Intel chipset fills differently.
fn fadt_intel(dsdt: u64) -> Vec<u8> {
    let mut t = fadt_minipc(dsdt);
    let mut put = |o: usize, v: &[u8]| t[o..o + v.len()].copy_from_slice(v);
    put(56, &0x1800u32.to_le_bytes()); // PM1a_EVT
    put(64, &0x1804u32.to_le_bytes()); // PM1a_CNT
    put(76, &0x1808u32.to_le_bytes()); // PM_TMR
    put(80, &0x1860u32.to_le_bytes()); // GPE0
    put(88, &[4, 2, 0, 4, 0x20, 0]); // lengths: PM1 event, PM1 control, PM2, timer, GPE0 (16 status + 16 enable), GPE1
    put(109, &(boot_arch::LEGACY_DEVICES | boot_arch::I8042).to_le_bytes());
    put(112, &(flags::WBINVD | flags::RESET_REG_SUP).to_le_bytes()); // no TMR_VAL_EXT: 24 bits; no Low Power S0
    put(116, &gas(1, 8, 1, 0xCF9));
    put(128, &[0x06]);
    put(148, &gas(1, 32, 3, 0x1800));
    put(172, &gas(1, 16, 2, 0x1804));
    put(208, &gas(1, 32, 3, 0x1808));
    put(220, &gas(1, 0xFF, 1, 0x1860));
    fix_checksum(&mut t, 9);
    t
}

fn world() -> FakeMem {
    let mut m = minipc();
    let list = [at::FADT, at::MADT, at::HPET, at::MCFG, at::SSDT1, at::SSDT2, DMAR_AT];
    m.put(at::XSDT, xsdt(&list));
    m.put(at::RSDT, rsdt(&list.map(|x| x as u32)));
    m.put(at::FADT, fadt_intel(at::DSDT));
    m.put(at::MADT, gp_platform::madt::gaming_pc());
    m.put(at::MCFG, mcfg(&[(0xC000_0000, 0, 0, 0xDF)]));
    m.put(DMAR_AT, gp_platform::dmar::gaming_pc());
    m.put(at::DSDT, dsdt(&s5_aml(7, 0)));
    m
}

#[test]
fn the_table_set_reads_clean() {
    let a = parse(&mut world(), at::RSDP);
    assert!(a.notes.is_empty(), "{:?}", a.notes);
    let m = a.madt.as_ref().unwrap();
    assert_eq!((m.enabled_cpus(), m.empty_slots, m.ioapics.len()), (20, 28, 1));
    assert_eq!(a.dmar.as_ref().unwrap().units.len(), 2);
    assert_eq!(a.mcfg[0].window(), (0xC000_0000, 0xCE00_0000), "buses 00-df");
    assert!(a.ivrs.is_none() && a.srat.is_none());
    let f = a.fadt.as_ref().unwrap();
    assert_eq!(f.pm_timer_port(), Some((0x1808, 24)), "the port Linux names, 24 bits without TMR_VAL_EXT");
    assert!(f.pm_timer_port().unwrap().0 >= crate::clock::PM_PORT_MIN);
    assert_eq!(f.has_8042(), Some(true));
    assert!(!f.hw_reduced() && !f.low_power_s0());
}

#[test]
fn reboot_goes_through_the_fadt_s_cf9() {
    let a = parse(&mut world(), at::RSDP);
    let p = reboot_plan(a.fadt.as_ref());
    let (hard, full) = (Step::Cf9 { code: 0x06 }, Step::Cf9 { code: 0x0E });
    assert_eq!(p.steps, alloc::vec![Step::FadtCf9 { value: 0x06 }, hard, full.clone(), Step::Kbc { declared: true }, Step::TripleFault]);
    assert!(p.declared() && p.skipped.is_empty());
    // A stick that asked for a cold reset gets the power-cycling code first, the firmware's own path right after.
    assert_eq!(p.cold_first().steps[..2], [full, Step::FadtCf9 { value: 0x06 }]);
}

#[test]
fn power_off_writes_the_intel_blocks() {
    let a = parse(&mut world(), at::RSDP);
    assert_eq!(a.s5.map(|s| (s.0.typ_a, s.0.typ_b)), Some((7, 0)), "Intel's S5 type from the DSDT, not a built-in 5");
    // The firmware hands over in legacy mode (SCI_EN clear), as the Mini-PC's does: ACPI mode is asked for first.
    let p = poweroff_plan(&a, Some(false)).unwrap();
    assert_eq!(p.acpi_enable, Some((0xB2, 0xA0)));
    assert_eq!(p.pm1a_cnt.map(|g| (g.space, g.address)), Some((Space::Io, 0x1804)));
    assert_eq!(p.pm1a_evt.map(|g| g.address), Some(0x1800));
    assert_eq!((p.pm1b_cnt, p.sleep_control), (None, None));
    assert_eq!(p.gpes.len(), 1);
    assert_eq!((p.gpes[0].status, p.gpes[0].enable, p.gpes[0].len), (0x1860, 0x1870, 16), "128 general events: 16 status and 16 enable bytes");
    assert_eq!(crate::power::pm1_value(0x0001, 7, true), 0x3C01, "SLP_TYP 7 and SLP_EN, SCI_EN kept");
    // Already in ACPI mode: no SMI command.
    assert_eq!(poweroff_plan(&a, Some(true)).unwrap().acpi_enable, None);
}
