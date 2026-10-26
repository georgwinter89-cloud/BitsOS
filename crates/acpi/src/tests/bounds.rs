//! Checker round: work and memory stay bounded for hostile tables (budget, counted notes, per-kind flags),
//! register pages are never read as tables, and the FADT/MADT rules Linux applies.

#[allow(unused_imports)]
use alloc::{string::String, vec, vec::Vec};
use crate::mem::{MemError, PhysMem, TABLE_MAX};
use crate::parse::{parse, parse_with_budget};
use crate::ranges::RangeSet;
use crate::testkit::*;
use crate::{Acpi, Note, Notes};

fn has(a: &Acpi, what: &str, sig: &[u8; 4]) -> bool {
    a.notes.iter().any(|n| n.what == what && &n.signature == sig)
}

#[test]
fn notes_count_repeats_instead_of_growing() {
    let mut n = Notes::new();
    for i in 0..10_000u64 {
        n.push(Note::table(*b"APIC", 0x1000, "entry_short", i));
    }
    n.push(Note::field(*b"APIC", 0x1000, "entry_short", "x", 0));
    n.push(Note::table(*b"HPET", 0x2000, "entry_short", 0));
    assert_eq!(n.len(), 3);
    assert_eq!((n[0].count, n[0].detail), (10_000, 0), "first detail kept");
    assert_eq!(n.total(), 10_002);
}

#[test]
fn madt_of_a_million_tiny_entries_gives_one_note() {
    // 2-byte type-0 entries: each is too short for a CPU; a note per entry would be millions of notes.
    let mut b = vec![0u8; 8];
    b.extend((0..1_000_000).flat_map(|_| [0u8, 2]));
    let t = sdt(b"APIC", 5, &b);
    let mut n = Notes::new();
    let m = crate::madt::parse(&t, 0, &mut n).unwrap();
    assert!(m.cpus.is_empty());
    assert_eq!(n.len(), 1);
    assert_eq!((n[0].what, n[0].count), ("entry_short", 1_000_000));
}

/// One FADT, listed `n` times: a root table of a quarter of a million entries.
fn world_with_long_xsdt(n: usize) -> FakeMem {
    // Far from the other tables: FakeMem would let them shadow a 2 MB blob at the usual place.
    const FAR: u64 = at::BASE + 0x10_0000;
    let mut m = minipc();
    let mut list = vec![at::FADT; n];
    list.push(at::MADT);
    m.put(FAR, xsdt(&list));
    m.put(at::RSDP, rsdp(2, at::RSDT as u32, FAR));
    m
}

#[test]
fn huge_root_table_is_linear_and_quiet() {
    let mut m = world_with_long_xsdt(250_000);
    let a = parse(&mut m, at::RSDP);
    assert!(a.fadt.is_some() && a.madt.is_some());
    assert_eq!(a.tables.len(), 250_001);
    let dup: Vec<&Note> = a.notes.iter().filter(|n| n.what == "duplicate").collect();
    assert_eq!(dup.len(), 1);
    assert_eq!(dup[0].count, 249_999);
    assert!(a.notes.len() < 5, "{:?}", a.notes);
}

#[test]
fn budget_stops_the_walk_with_one_note() {
    let mut m = world_with_long_xsdt(250_000);
    // Room for the 2 MB XSDT and some of the 250 000 headers (9 MB), not for all of them.
    let a = parse_with_budget(&mut m, at::RSDP, 3 << 20);
    assert!(m.bytes_read <= 3 << 20, "read {}", m.bytes_read);
    assert_eq!(a.root, Some(crate::RootKind::Xsdt));
    assert!(has(&a, "budget_spent", b"XSDT"), "{:?}", a.notes);
    assert!(a.fadt.is_some(), "the first entries were still read");
    assert!(a.madt.is_none(), "the last entry was beyond the budget");
    // A root table bigger than the whole budget is not read at all: the RSDT serves.
    let mut m = world_with_long_xsdt(250_000);
    let a = parse_with_budget(&mut m, at::RSDP, 64 << 10);
    assert!(m.bytes_read <= 64 << 10, "read {}", m.bytes_read);
    assert_eq!(a.root, Some(crate::RootKind::Rsdt));
    assert!(has(&a, "budget_spent", b"XSDT") && a.madt.is_some(), "{:?}", a.notes);
}

#[test]
fn budget_refuses_a_big_ssdt_before_allocating_it() {
    // The DSDT has no _S5; an SSDT claims 8 MiB but only 64 KiB of budget are left.
    let mut m = minipc();
    m.put(at::DSDT, dsdt(&[0x14, 0x05, b'_', b'P', b'T', b'S', 0x01]));
    if let Some(t) = m.get_mut(at::SSDT1) {
        t[4..8].copy_from_slice(&(8u32 << 20).to_le_bytes());
    }
    let a = parse_with_budget(&mut m, at::RSDP, 64 << 10);
    assert!(m.bytes_read < 64 << 10);
    assert!(a.notes.iter().any(|n| n.what == "budget_spent" && &n.signature == b"SSDT"), "{:?}", a.notes);
    assert!(a.s5.is_none());
    // The default budget is far above any real firmware's tables.
    assert!(crate::mem::PARSE_BUDGET >= 2 * TABLE_MAX as u64);
}

#[test]
fn ssdt_with_an_insane_listed_length_is_never_loaded() {
    let mut m = minipc();
    m.put(at::DSDT, dsdt(&[0x14, 0x05, b'_', b'P', b'T', b'S', 0x01]));
    if let Some(t) = m.get_mut(at::SSDT1) {
        t[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    }
    m.put(at::SSDT2, sdt(b"SSDT", 2, &s5_aml(5, 5)));
    let before = m.bytes_read;
    let a = parse(&mut m, at::RSDP);
    assert_eq!(a.s5.map(|s| s.2), Some(at::SSDT2));
    assert!(m.bytes_read - before < 64 << 10);
}

#[test]
fn cut_s5_in_a_bad_dsdt_falls_back_to_an_ssdt() {
    // A DSDT with a bad checksum whose _S5 package is cut off, an SSDT with a good one: the SSDT's counts.
    let mut m = minipc();
    let mut d = dsdt(&[0x08, b'_', b'S', b'5', b'_', 0x12, 0x3F, 0x02, 0x0A, 5]);
    d[36] ^= 0x01;
    m.put(at::DSDT, d);
    m.put(at::SSDT2, sdt(b"SSDT", 2, &s5_aml(3, 3)));
    let a = parse(&mut m, at::RSDP);
    assert!(has(&a, "bad_checksum", b"DSDT"));
    let (s5, sig, p) = a.s5.unwrap();
    assert_eq!((s5.typ_a, &sig, p), (3, b"SSDT", at::SSDT2));
    // The same SSDT with a bad checksum is not used: no _S5 at all.
    m.get_mut(at::SSDT2).unwrap()[40] ^= 0x01;
    let a = parse(&mut m, at::RSDP);
    assert!(a.s5.is_none() && has(&a, "bad_checksum", b"SSDT"), "{:?}", a.notes);
}

/// A reader that refuses what the parser told it to avoid, like `KernelMem` does for reserved pages.
struct Avoiding {
    inner: FakeMem,
    avoid: RangeSet,
}

impl PhysMem for Avoiding {
    fn read(&mut self, phys: u64, out: &mut [u8]) -> Result<(), MemError> {
        if self.avoid.overlaps(phys, phys.saturating_add(out.len() as u64)) {
            return Err(MemError::Guarded);
        }
        self.inner.read(phys, out)
    }
    fn avoid(&mut self, start: u64, end: u64) {
        self.avoid.insert(start, end);
    }
}

#[test]
fn register_pages_named_by_tables_are_not_read_as_tables() {
    let mut m = Avoiding { inner: minipc(), avoid: RangeSet::new() };
    // An SSDT "found" inside the HPET's register page, listed after the HPET.
    let mut list = vec![at::FADT, at::MADT, at::HPET, at::MCFG, at::IVRS, at::SRAT, 0xFED0_0000, at::SSDT2];
    m.inner.put(at::XSDT, xsdt(&list));
    m.inner.put(0xFED0_0000, sdt(b"SSDT", 2, &s5_aml(1, 1)));
    m.inner.put(at::DSDT, dsdt(&[0x14, 0x05, b'_', b'P', b'T', b'S', 0x01]));
    let a = parse(&mut m, at::RSDP);
    for (s, e) in [(0xFED0_0000u64, 0xFED0_1000u64), (0xFEC0_0000, 0xFEC0_2000), (0xFEE0_0000, 0xFEE0_1000), (0xE000_0000, 0xF000_0000), (0xFD20_0000, 0xFD28_0000)] {
        assert!(m.avoid.overlaps(s, s + 1) && m.avoid.overlaps(e - 1, e), "{s:#x}..{e:#x} not avoided");
    }
    assert!(!m.avoid.overlaps(at::BASE, at::BASE + off::SPAN), "the tables themselves stay readable");
    assert!(a.s5.is_none(), "the SSDT in the HPET page was read");
    assert!(has(&a, "guarded", b"????"), "{:?}", a.notes);
    // Listed before the HPET, the same page cannot be told from a table yet (documented limit).
    list.swap(2, 6);
    m.inner.put(at::XSDT, xsdt(&list));
    m.avoid = RangeSet::new();
    let a = parse(&mut m, at::RSDP);
    assert!(a.hpet.is_some());
}

#[test]
fn range_set_merges_and_finds() {
    let mut r = RangeSet::new();
    r.insert(0x3000, 0x4000);
    r.insert(0x1000, 0x2000);
    r.insert(0x2000, 0x3000);
    assert_eq!(r.len(), 1, "touching ranges merge");
    r.insert(0x8000, 0x9000);
    r.insert(0x0, 0x0);
    assert_eq!(r.len(), 2);
    assert!(r.overlaps(0x3FFF, 0x5000) && !r.overlaps(0x4000, 0x8000) && r.overlaps(0x7000, 0x8001));
    r.insert(0x500, 0xA000);
    assert_eq!(r.len(), 1, "a range over several swallows them");
    assert!(r.overlaps(0x9FFF, 0xA000) && !r.overlaps(0xA000, u64::MAX) && !r.overlaps(5, 5));
    r.insert(u64::MAX - 1, u64::MAX);
    assert!(r.overlaps(u64::MAX - 1, u64::MAX));
}

#[test]
fn fadt_revision_2_has_no_boot_arch_and_reset_must_be_one_byte() {
    // Revision 2 (ACPI 1.0 + reset register): offset 109 is reserved, maybe garbage.
    let mut f = fadt_minipc(at::DSDT);
    f[8] = 2;
    f[109] = 0x02;
    fix_checksum(&mut f, 9);
    let x = crate::fadt::parse(&f, 0, &mut Notes::new()).unwrap();
    assert_eq!((x.has_8042(), x.legacy_devices(), x.boot_arch), (None, None, 0));
    assert!(x.usable_reset().is_some(), "the reset register exists from revision 2");
    // Revision 3 reads it.
    f[8] = 3;
    fix_checksum(&mut f, 9);
    assert_eq!(crate::fadt::parse(&f, 0, &mut Notes::new()).unwrap().has_8042(), Some(true));
    // A 32-bit reset register, or one at bit 4: noted, not used.
    for (width, offset) in [(32u8, 0u8), (16, 0), (8, 4)] {
        let mut f = fadt_minipc(at::DSDT);
        f[117] = width;
        f[118] = offset;
        fix_checksum(&mut f, 9);
        let mut n = Notes::new();
        let x = crate::fadt::parse(&f, 0, &mut n).unwrap();
        assert!(x.reset.is_none() && x.usable_reset().is_none(), "{width}/{offset}");
        assert!(n.iter().any(|n| n.what == "reset_width" && n.field == "reset"), "{n:?}");
    }
    // Width 0 (unspecified) at bit 0 is accepted as one byte.
    let mut f = fadt_minipc(at::DSDT);
    f[117] = 0;
    fix_checksum(&mut f, 9);
    assert!(crate::fadt::parse(&f, 0, &mut Notes::new()).unwrap().usable_reset().is_some());
}

#[test]
fn madt_broadcast_apic_ids_are_not_cpus() {
    let mut b = Vec::new();
    b.extend_from_slice(&0xFEE0_0000u32.to_le_bytes());
    b.extend_from_slice(&0u32.to_le_bytes());
    b.extend_from_slice(&[0, 8, 0, 0, 1, 0, 0, 0]);
    b.extend_from_slice(&[0, 8, 1, 0xFF, 1, 0, 0, 0]);
    b.extend_from_slice(&[9, 16, 0, 0]);
    b.extend_from_slice(&u32::MAX.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&2u32.to_le_bytes());
    let mut n = Notes::new();
    let m = crate::madt::parse(&sdt(b"APIC", 5, &b), 0, &mut n).unwrap();
    assert_eq!(m.cpus.len(), 1);
    assert_eq!(m.enabled_cpus(), 1);
    let bc = n.iter().find(|x| x.what == "broadcast_apic_id").unwrap();
    assert_eq!((bc.count, bc.detail), (2, 0xFF));
}
