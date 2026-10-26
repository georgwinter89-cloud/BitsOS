//! One thing broken at a time: the rest still parses, the problem is a note, nothing panics.

#[allow(unused_imports)]
use alloc::{string::String, vec, vec::Vec};
use crate::testkit::*;
use crate::parse::parse;
use crate::{Acpi, RootKind};

fn notes(a: &Acpi) -> Vec<(&'static str, String)> {
    a.notes.iter().map(|n| (n.what, crate::bytes::text(&n.signature))).collect()
}

fn has(a: &Acpi, what: &str, sig: &str) -> bool {
    notes(a).iter().any(|(w, s)| *w == what && s == sig)
}

#[test]
fn bad_checksum_skips_that_table_only() {
    let mut m = minipc();
    m.get_mut(at::FADT).unwrap()[50] ^= 1;
    let a = parse(&mut m, at::RSDP);
    assert!(a.fadt.is_none());
    assert!(has(&a, "bad_checksum", "FACP"), "{:?}", notes(&a));
    assert!(a.madt.is_some() && a.hpet.is_some() && !a.mcfg.is_empty());
    let t = a.tables.iter().find(|t| &t.header.signature == b"FACP").unwrap();
    assert_eq!((t.checksum_ok, t.used), (Some(false), false));
    // Without the FADT there is no DSDT, so no _S5.
    assert!(a.s5.is_none());
}

#[test]
fn second_fadt_is_used_when_the_first_is_bad() {
    let mut m = minipc();
    m.get_mut(at::FADT).unwrap()[50] ^= 1;
    let second = 0x9A00_A000;
    m.put(second, fadt_minipc(at::DSDT));
    m.put(at::XSDT, xsdt(&[at::FADT, second, at::MADT, second]));
    let a = parse(&mut m, at::RSDP);
    assert!(a.fadt.is_some());
    assert!(a.s5.is_some());
    // The third listing (the good one again) is a duplicate.
    assert!(has(&a, "duplicate", "FACP"), "{:?}", notes(&a));
}

#[test]
fn rsdp_problems() {
    let mut m = minipc();
    m.get_mut(at::RSDP).unwrap()[33] ^= 1; // extended checksum only
    let a = parse(&mut m, at::RSDP);
    assert_eq!(a.root, Some(RootKind::Rsdt));
    assert!(has(&a, "bad_ext_checksum", "RSDP"));
    assert!(a.fadt.is_some());

    let mut m = minipc();
    m.get_mut(at::RSDP).unwrap()[16] ^= 1; // RSDT address: both checksums break
    let a = parse(&mut m, at::RSDP);
    assert!(a.root.is_none() && a.tables.is_empty());
    assert!(has(&a, "bad_checksum", "RSDP"));

    let mut m = minipc();
    m.get_mut(at::RSDP).unwrap()[0] = b'X';
    assert!(has(&parse(&mut m, at::RSDP), "bad_signature", "RSDP"));
    assert!(has(&parse(&mut minipc(), 0x1234), "unreadable", "RSDP"));
}

#[test]
fn xsdt_problems_fall_back_to_the_rsdt() {
    for broken in 0..3 {
        let mut m = minipc();
        match broken {
            0 => m.get_mut(at::XSDT).unwrap()[40] ^= 1,
            1 => m.blobs.retain(|(a, _)| *a != at::XSDT),
            _ => m.get_mut(at::XSDT).unwrap()[0] = b'Y',
        }
        let a = parse(&mut m, at::RSDP);
        assert_eq!(a.root, Some(RootKind::Rsdt), "case {broken}");
        assert!(has(&a, "fallback_rsdt", "XSDT"), "case {broken}: {:?}", notes(&a));
        assert!(a.fadt.is_some() && a.madt.is_some());
    }
}

#[test]
fn root_entries_null_unreadable_odd_length() {
    let mut m = minipc();
    let mut x = xsdt(&[0, 0xDEAD_0000, at::FADT]);
    x.extend_from_slice(&[1, 2, 3]);
    let len = x.len() as u32;
    x[4..8].copy_from_slice(&len.to_le_bytes());
    fix_checksum(&mut x, 9);
    m.put(at::XSDT, x);
    let a = parse(&mut m, at::RSDP);
    assert!(has(&a, "null_entry", "XSDT"));
    assert!(has(&a, "odd_length", "XSDT"));
    assert!(has(&a, "not_table_memory", "????"));
    assert!(a.fadt.is_some());
}

#[test]
fn truncated_and_hostile_lengths() {
    // Header says 20 bytes (< 36), 0 bytes, and 4 GiB: refused, and the 4 GiB one is never read.
    for (len, what) in [(20u32, "too_short"), (0, "too_short"), (u32::MAX, "too_long")] {
        let mut m = minipc();
        let f = m.get_mut(at::MADT).unwrap();
        f[4..8].copy_from_slice(&len.to_le_bytes());
        let a = parse(&mut m, at::RSDP);
        assert!(a.madt.is_none());
        assert!(has(&a, what, "APIC"), "{len}: {:?}", notes(&a));
        assert!(m.bytes_read < 64 * 1024, "{len}: read {} bytes", m.bytes_read);
    }
    // Length longer than the memory behind it: unreadable.
    let mut m = minipc();
    let f = m.get_mut(at::HPET).unwrap();
    f[4..8].copy_from_slice(&4000u32.to_le_bytes());
    let a = parse(&mut m, at::RSDP);
    assert!(a.hpet.is_none());
    assert!(has(&a, "not_table_memory", "HPET"), "{:?}", notes(&a));
    // Lengths below what the type needs, with a good checksum.
    let mut m = minipc();
    m.put(at::MADT, sdt(b"APIC", 5, &[0; 4]));
    m.put(at::FADT, sdt(b"FACP", 6, &[0; 40]));
    m.put(at::HPET, sdt(b"HPET", 1, &[0; 10]));
    let a = parse(&mut m, at::RSDP);
    assert!(a.madt.is_none() && a.fadt.is_none() && a.hpet.is_none());
    assert!(has(&a, "too_short", "APIC") && has(&a, "too_short", "FACP") && has(&a, "too_short", "HPET"));
}

#[test]
fn table_across_pages() {
    // The FADT split over two adjacent pieces of memory (two pages, two descriptors) reads as one table.
    let mut m = minipc();
    let f = fadt_minipc(at::DSDT);
    let at_fadt = 0x9A00_BF80; // 128 bytes before a page boundary
    m.put(at_fadt, f[..128].to_vec());
    m.put(at_fadt + 128, f[128..].to_vec());
    m.put(at::XSDT, xsdt(&[at_fadt, at::MADT]));
    let a = parse(&mut m, at::RSDP);
    assert!(a.fadt.is_some(), "{:?}", notes(&a));
    // Second page missing: unreadable, noted.
    m.blobs.retain(|(a, _)| *a != at_fadt + 128);
    let a = parse(&mut m, at::RSDP);
    assert!(a.fadt.is_none());
    assert!(has(&a, "not_table_memory", "FACP"), "{:?}", notes(&a));
}

#[test]
fn dsdt_bad_checksum_still_gives_s5_with_a_note() {
    let mut m = minipc();
    m.get_mut(at::DSDT).unwrap()[37] ^= 0x40;
    let a = parse(&mut m, at::RSDP);
    assert!(has(&a, "bad_checksum", "DSDT"));
    assert_eq!(a.s5.map(|s| s.0.typ_a), Some(5));
    assert_eq!(a.dsdt.unwrap().checksum_ok, Some(false));
    // DSDT absent: noted, no S5 (the SSDTs have none).
    m.blobs.retain(|(a, _)| *a != at::DSDT);
    let a = parse(&mut m, at::RSDP);
    assert!(a.s5.is_none() && has(&a, "not_table_memory", "DSDT"));
}

#[test]
fn madt_bad_entries() {
    let base = madt_minipc();
    // An entry of length 0 after the 16 CPUs: the walk stops (no endless loop), the CPUs stay.
    let mut b = base[36..36 + 8 + 16 * 8].to_vec();
    b.extend_from_slice(&[1, 0, 0, 0]);
    b.extend_from_slice(&[0, 4, 0, 0]);
    let t = sdt(b"APIC", 5, &b);
    let mut n = crate::Notes::new();
    let md = crate::madt::parse(&t, 0, &mut n).unwrap();
    assert_eq!(md.cpus.len(), 16);
    assert!(md.ioapics.is_empty());
    assert!(n.iter().any(|x| x.what == "entry_length"));
    // A short type-1 entry (4 bytes) is skipped, the walk goes on; a length past the end stops it.
    let mut b = base[36..44].to_vec();
    b.extend_from_slice(&[1, 4, 0, 0]);
    b.extend_from_slice(&[0, 8, 1, 1, 1, 0, 0, 0]);
    b.extend_from_slice(&[0, 200, 2, 2, 1, 0, 0, 0]);
    let t = sdt(b"APIC", 5, &b);
    let mut n = crate::Notes::new();
    let md = crate::madt::parse(&t, 0, &mut n).unwrap();
    assert_eq!(md.cpus.len(), 1);
    assert!(n.iter().any(|x| x.what == "entry_short") && n.iter().any(|x| x.what == "entry_length"));
    // Two enabled CPUs with one APIC ID.
    let mut b = base[36..44].to_vec();
    b.extend_from_slice(&[0, 8, 0, 3, 1, 0, 0, 0, 0, 8, 1, 3, 1, 0, 0, 0]);
    let mut n = crate::Notes::new();
    crate::madt::parse(&sdt(b"APIC", 5, &b), 0, &mut n).unwrap();
    assert!(n.iter().any(|x| x.what == "duplicate_apic_id" && x.detail == 3));
}

#[test]
fn mcfg_ivrs_srat_bad_entries() {
    let mut n = crate::Notes::new();
    let e = crate::mcfg::parse(&mcfg(&[(0xE000_0000, 0, 10, 5), (0, 0, 0, 255), (0xF000_0000, 0, 0, 63)]), 0, &mut n);
    assert_eq!(e.len(), 1);
    assert_eq!(e[0].window(), (0xF000_0000, 0xF400_0000));
    assert_eq!(n.iter().find(|x| x.what == "bad_entry").map(|x| x.count), Some(2));

    let mut t = ivrs_minipc();
    t[48 + 2..48 + 4].copy_from_slice(&0u16.to_le_bytes()); // IVHD length 0
    let mut n = crate::Notes::new();
    let r = crate::ivrs::parse(&t, 0, &mut n).unwrap();
    assert!(r.iommus.is_empty() && n.iter().any(|x| x.what == "block_length"));

    let mut t = srat_one_domain();
    t[48 + 1] = 0; // first entry length 0
    let mut n = crate::Notes::new();
    let s = crate::srat::parse(&t, 0, &mut n).unwrap();
    assert!(s.cpus.is_empty() && n.iter().any(|x| x.what == "entry_length"));
}

#[test]
fn fadt_register_disagreements_are_noted() {
    let mut f = fadt_minipc(at::DSDT);
    f[64..68].copy_from_slice(&0x404u32.to_le_bytes()); // legacy PM1a_CNT != X_PM1a_CNT
    f[40..44].copy_from_slice(&0x1000u32.to_le_bytes()); // DSDT != X_DSDT
    fix_checksum(&mut f, 9);
    let mut n = crate::Notes::new();
    let x = crate::fadt::parse(&f, 0, &mut n).unwrap();
    assert_eq!(x.pm1a_cnt.unwrap().address, 0x804, "X_ wins");
    assert_eq!(x.dsdt, at::DSDT, "X_DSDT wins");
    assert!(n.iter().any(|x| x.what == "x_mismatch" && x.field == "pm1a_cnt"));
    assert!(n.iter().any(|x| x.what == "dsdt_mismatch"));
    // Reset register present but the flag off: kept for hw_info, not usable.
    let mut f = fadt_minipc(at::DSDT);
    let flags = u32::from_le_bytes([f[112], f[113], f[114], f[115]]) & !crate::fadt::flags::RESET_REG_SUP;
    f[112..116].copy_from_slice(&flags.to_le_bytes());
    fix_checksum(&mut f, 9);
    let x = crate::fadt::parse(&f, 0, &mut crate::Notes::new()).unwrap();
    assert!(x.reset.is_some() && x.usable_reset().is_none());
}
