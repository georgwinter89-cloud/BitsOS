#[allow(unused_imports)]
use alloc::{string::String, vec, vec::Vec};
use crate::testkit::*;
use crate::madt::{Polarity, Trigger};
use crate::parse::parse;
use crate::sdt::Space;
use crate::RootKind;

#[test]
fn minipc_tables() {
    let mut m = minipc();
    let a = parse(&mut m, at::RSDP);
    assert!(a.notes.is_empty(), "notes: {:?}", a.notes);
    let r = a.rsdp.unwrap();
    assert_eq!((r.revision, &r.oem_id, r.checksum_ok, r.ext_checksum_ok), (2, b"ALASKA", true, true));
    assert_eq!(a.root, Some(RootKind::Xsdt));
    assert_eq!(a.tables.len(), 8);
    assert_eq!(a.count(b"SSDT"), 2);
    assert!(a.has(b"IVRS"));

    let f = a.fadt.as_ref().unwrap();
    assert_eq!((f.revision, f.length, f.minor_revision), (6, 276, 1));
    let reset = f.usable_reset().unwrap();
    assert_eq!((reset.reg.space, reset.reg.address, reset.value), (Space::Io, 0xB2, 0xBE));
    assert_eq!(f.pm1a_cnt.unwrap().address, 0x804);
    assert_eq!(f.pm1b_cnt, None);
    assert_eq!(f.pm_timer_port(), Some((0x808, 32)));
    assert_eq!((f.smi_cmd, f.acpi_enable), (0xB2, 0xA0));
    assert!(f.low_power_s0() && !f.hw_reduced());
    assert_eq!(f.has_8042(), Some(false));
    assert_eq!(f.legacy_devices(), Some(false));
    assert_eq!(f.dsdt, at::DSDT);

    let md = a.madt.as_ref().unwrap();
    assert_eq!(md.lapic_address, 0xFEE0_0000);
    assert_eq!((md.cpus.len(), md.enabled_cpus()), (16, 16));
    assert!(md.has_8259());
    assert_eq!(md.ioapics.len(), 2);
    assert_eq!((md.ioapics[0].id, md.ioapics[0].address, md.ioapics[0].gsi_base), (33, 0xFEC0_0000, 0));
    assert_eq!((md.ioapics[1].id, md.ioapics[1].address, md.ioapics[1].gsi_base), (34, 0xFEC0_1000, 24));
    assert_eq!(md.ioapic_for(30).unwrap().id, 34);
    assert_eq!(md.isa_override(0).unwrap().gsi, 2);
    let sci = md.isa_override(9).unwrap();
    assert_eq!((sci.flags.polarity(), sci.flags.trigger()), (Polarity::Low, Trigger::Level));
    assert_eq!((md.lapic_nmis[0].uid, md.lapic_nmis[0].lint), (None, 1));

    let h = a.hpet.unwrap();
    assert_eq!((h.mmio_base(), h.vendor(), h.comparators(), h.counter_64bit(), h.legacy_capable()), (Some(0xFED0_0000), 0x1022, 3, false, true));
    assert_eq!(a.mcfg.len(), 1);
    assert_eq!(a.mcfg[0].window(), (0xE000_0000, 0xF000_0000));
    let iv = a.ivrs.as_ref().unwrap();
    assert_eq!((iv.iommus.len(), iv.iommus[0].base, iv.iommus[0].device_id), (1, 0xFD20_0000, 2));
    let s = a.srat.as_ref().unwrap();
    assert_eq!((s.domains(), s.cpus.len(), s.memory.len()), (1, 2, 1));
    assert_eq!(s.cpus[1].apic_id, 300);
    assert_eq!(s.memory[0].length, 1 << 32);

    let (s5, sig, p) = a.s5.unwrap();
    assert_eq!((s5.typ_a, s5.typ_b, &sig, p), (5, 5, b"DSDT", at::DSDT));
    assert_eq!(a.dsdt.unwrap().checksum_ok, Some(true));
}

#[test]
fn summary_has_the_check_list_values() {
    let mut m = minipc();
    let a = parse(&mut m, at::RSDP);
    let s = crate::report::summary(&a, Some(false)).join("\n");
    for want in ["reset io:0xb2<-0xbe", "pm1a_cnt io:0x804/16", "SCI_EN at boot no", "I/O APIC id 33 at 0xfec00000", "I/O APIC id 34 at 0xfec01000, gsi base 24", "MCFG ECAM 0xe0000000 segment 0 buses 0-255", "_S5 typ_a 5", "8042 no", "low_power_s0 true", "SSDTx2", "IVRS iommus 1 [0xfd200000@00:00.2]"] {
        assert!(s.contains(want), "missing {want:?} in\n{s}");
    }
}

#[test]
fn rsdp_revision_0_uses_the_rsdt() {
    let mut m = minipc();
    let mut r = rsdp(0, at::RSDT as u32, 0);
    r.truncate(20);
    m.put(at::RSDP, r);
    let a = parse(&mut m, at::RSDP);
    assert_eq!(a.root, Some(RootKind::Rsdt));
    assert!(a.fadt.is_some() && a.madt.is_some());
    assert!(a.notes.is_empty(), "{:?}", a.notes);
}

#[test]
fn s5_in_an_ssdt_when_the_dsdt_has_none() {
    let mut m = minipc();
    m.put(at::DSDT, dsdt(&[0x14, 0x05, b'_', b'P', b'T', b'S', 0x01]));
    m.put(at::SSDT2, sdt(b"SSDT", 2, &s5_aml(7, 7)));
    let a = parse(&mut m, at::RSDP);
    let (s5, sig, p) = a.s5.unwrap();
    assert_eq!((s5.typ_a, &sig, p), (7, b"SSDT", at::SSDT2));
}

#[test]
fn s5_forms() {
    let t = |aml: &[u8]| crate::s5::find(&sdt(b"DSDT", 2, aml));
    // Root-prefixed name, ZeroOp/OneOp, one element only.
    assert_eq!(t(&[0x08, b'\\', b'_', b'S', b'5', b'_', 0x12, 0x04, 0x02, 0x01, 0x00]).map(|s| (s.typ_a, s.typ_b)), Some((1, 0)));
    assert_eq!(t(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x04, 0x01, 0x0A, 0x05]).map(|s| (s.typ_a, s.typ_b)), Some((5, 5)));
    // Word and dword prefixes.
    assert_eq!(t(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x0A, 0x02, 0x0B, 3, 0, 0x0C, 4, 0, 0, 0]).map(|s| (s.typ_a, s.typ_b)), Some((3, 4)));
    // Two-byte PkgLength (0x40 | low nibble, then the next 8 bits).
    let long = vec![0x08, b'_', b'S', b'5', b'_', 0x12, 0x40 | 0x07, 0x00, 0x02, 0x0A, 6, 0x0A, 6];
    assert_eq!(t(&long).map(|s| s.typ_a), Some(6));
    // A Method returning a package is not a Name: refused; the later good Name still counts.
    let mut m = vec![0x14, 0x0A, b'_', b'S', b'5', b'_', 0x00, 0xA4, 0x12, 0x03, 0x01, 0x01];
    m.extend_from_slice(&s5_aml(2, 2));
    assert_eq!(t(&m).map(|s| s.typ_a), Some(2));
    // Value above 7, empty package, cut-off package: refused.
    assert_eq!(t(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x04, 0x01, 0x0A, 8]), None);
    assert_eq!(t(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x02, 0x00]), None);
    assert_eq!(t(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x3F, 0x02, 0x0A, 5]), None);
}

#[test]
fn madt_x2apic_entries_and_override() {
    let mut b = Vec::new();
    b.extend_from_slice(&0xFEE0_0000u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    // x2APIC CPU 300 enabled, x2APIC CPU 301 online-capable only, LAPIC address override, x2APIC NMI all.
    for (id, f) in [(300u32, 1u32), (301, 2)] {
        b.extend_from_slice(&[9, 16, 0, 0]);
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&f.to_le_bytes());
        b.extend_from_slice(&id.to_le_bytes());
    }
    b.extend_from_slice(&[5, 12, 0, 0]);
    b.extend_from_slice(&0x1_FEE0_0000u64.to_le_bytes());
    b.extend_from_slice(&[0xA, 12, 0x0D, 0]);
    b.extend_from_slice(&u32::MAX.to_le_bytes());
    b.extend_from_slice(&[1, 0, 0, 0]);
    let t = sdt(b"APIC", 5, &b);
    let mut notes = crate::Notes::new();
    let m = crate::madt::parse(&t, 0, &mut notes).unwrap();
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(m.lapic_address, 0x1_FEE0_0000);
    assert_eq!(m.cpus.len(), 2);
    assert!(m.cpus.iter().all(|c| c.x2apic));
    assert_eq!((m.cpus[1].enabled, m.cpus[1].online_capable), (false, true));
    assert_eq!(m.enabled_cpus(), 1);
    assert_eq!((m.lapic_nmis[0].uid, m.lapic_nmis[0].lint), (None, 1));
    assert!(!m.has_8259());
}

#[test]
fn fadt_legacy_ports_without_x_fields() {
    // An ACPI 1.0 FADT (116 bytes): blocks only as ports, no reset register, boot_arch unknown.
    let mut f = fadt_minipc(at::DSDT);
    f.truncate(116);
    f[4..8].copy_from_slice(&116u32.to_le_bytes());
    f[8] = 1;
    fix_checksum(&mut f, 9);
    let mut notes = crate::Notes::new();
    let x = crate::fadt::parse(&f, 0, &mut notes).unwrap();
    assert_eq!(x.pm1a_cnt.unwrap().space, Space::Io);
    assert_eq!(x.pm1a_cnt.unwrap().bit_width, 16);
    assert_eq!(x.reset, None);
    assert_eq!(x.has_8042(), None);
    assert_eq!(x.dsdt, at::DSDT & 0xFFFF_FFFF);
}
