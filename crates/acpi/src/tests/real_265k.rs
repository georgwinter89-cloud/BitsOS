//! The Gaming-PC's real MADT (Intel Core Ultra 7 265K on an ASRock Z890 Taichi OCF, read on 5.11.2026): x2APIC entries
//! only, sparse APIC IDs, and 28 padding entries with the broadcast ID that are no CPUs.
//! The file is the raw table (no serial number, no address of a device); `gp_platform::madt::gaming_pc` builds the
//! same bytes for crates that must not carry a capture.

use crate::madt::{self, Polarity, Trigger};
use crate::Notes;
use alloc::vec::Vec;

const RAW: &[u8] = include_bytes!("data/gaming-pc-265k-madt.bin");

/// CPU order as in the table, which is Linux's CPU numbering on this machine.
const APIC_IDS: [u32; 20] = [0, 8, 32, 40, 48, 56, 80, 88, 24, 26, 28, 30, 64, 66, 68, 70, 72, 74, 76, 78];

#[test]
fn real_madt() {
    let mut notes = Notes::new();
    let m = madt::parse(RAW, 0, &mut notes).expect("the real table parses");
    assert_eq!(m.lapic_address, 0xFEE0_0000);
    assert!(m.has_8259(), "PCAT_COMPAT");
    assert_eq!(m.enabled_cpus(), 20);
    assert_eq!(m.cpus.iter().map(|c| c.apic_id).collect::<Vec<_>>(), APIC_IDS);
    assert!(m.cpus.iter().all(|c| c.enabled && c.x2apic), "only x2APIC entries, all enabled");
    assert_eq!(m.other_entries, 0);
    // Linux's CPUs 0-19 have these ACPI UIDs: the table's own numbering is not the CPU order.
    assert_eq!(m.cpus[8].uid, 2);
    assert_eq!(m.cpus[2].uid, 6);
    assert_eq!(m.cpus[7].uid, 19);

    assert_eq!(m.ioapics.len(), 1);
    assert_eq!((m.ioapics[0].id, m.ioapics[0].address, m.ioapics[0].gsi_base), (2, 0xFEC0_0000, 0));
    assert_eq!(m.overrides.len(), 2);
    assert_eq!((m.overrides[0].source, m.overrides[0].gsi), (0, 2));
    assert_eq!((m.overrides[1].source, m.overrides[1].gsi), (9, 9));
    assert_eq!((m.overrides[1].flags.polarity(), m.overrides[1].flags.trigger()), (Polarity::High, Trigger::Level));
    assert_eq!(m.lapic_nmis.len(), 1);
    assert_eq!((m.lapic_nmis[0].uid, m.lapic_nmis[0].lint), (None, 1), "x2APIC NMI for all CPUs on LINT1");
    // GP B6: the padding entries are unused slots, counted and not a problem; nothing is wrong with the table.
    assert_eq!(m.empty_slots, 28);
    let all: Vec<_> = notes.iter().collect();
    assert!(all.is_empty(), "{all:?}");
}

/// The builder the other crates' tests use gives this machine's table byte for byte. This is also the proof that
/// the committed capture names nothing private: every byte of it is produced by `gp_platform::madt::gaming_pc()`,
/// whose inputs (processor IDs, the interrupt controller's address, the vendor's table names) stand in the source.
#[test]
fn the_builder_is_the_real_table() {
    assert_eq!(gp_platform::madt::gaming_pc(), RAW);
}

/// The rule is narrow: the same ID with a flag set stays a problem, and the Mini-PC's table (16 xAPIC entries,
/// no padding) reads exactly as before.
#[test]
fn only_unused_slots_are_quiet() {
    use gp_platform::madt::{MadtBuilder, ENABLED, ONLINE_CAPABLE, X2APIC_BROADCAST};
    let t = MadtBuilder::new(0xFEE0_0000, 1)
        .x2apic(0, 0, ENABLED)
        .x2apic(1, X2APIC_BROADCAST, 0)
        .x2apic(2, X2APIC_BROADCAST, ENABLED)
        .x2apic(3, X2APIC_BROADCAST, ONLINE_CAPABLE)
        .x2apic(4, 9, 0)
        .build();
    let mut notes = Notes::new();
    let m = madt::parse(&t, 0, &mut notes).unwrap();
    assert_eq!((m.cpus.len(), m.enabled_cpus(), m.empty_slots), (2, 1, 1), "a disabled CPU with a real ID stays a CPU");
    let all: Vec<_> = notes.iter().collect();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!((all[0].what, all[0].count), ("broadcast_apic_id", 2));

    let mut notes = Notes::new();
    let m = madt::parse(&crate::testkit::madt_minipc(), 0, &mut notes).unwrap();
    assert_eq!((m.cpus.len(), m.empty_slots, notes.len()), (16, 0, 0));
}
