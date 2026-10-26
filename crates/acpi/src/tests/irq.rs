//! CP-3 host tests: the routing plan (Mini-PC and q35-like worlds, one broken thing at a time, hostile MADTs),
//! the NMI pin choice, I/O APIC entry encodings and the storm guard's counting.

#[allow(unused_imports)]
use alloc::{string::String, vec, vec::Vec};
use crate::fadt::boot_arch;
use crate::ioapic::{self, pins_from_version, Entry};
use crate::irq_plan::{isa_line, nmi_pin, plan, storm_clock, Line, PlanNote, Purpose, StormGuard};
use crate::madt::{IntiFlags, IoApic, LapicNmi, Madt, Override};
use crate::parse::parse;
use crate::testkit::*;
use crate::Acpi;
use hal::apic::NmiPin;

fn minipc_acpi() -> Acpi {
    let mut m = minipc();
    let a = parse(&mut m, at::RSDP);
    assert!(a.notes.is_empty());
    a
}

/// q35 as QEMU builds it: one I/O APIC, overrides 0->2 and 5/9/10/11 level active high, an 8042.
fn q35_acpi() -> Acpi {
    let mut a = minipc_acpi();
    let f = a.fadt.as_mut().unwrap();
    f.boot_arch |= boot_arch::I8042;
    let m = a.madt.as_mut().unwrap();
    m.ioapics = vec![IoApic { id: 0, address: 0xFEC0_0000, gsi_base: 0 }];
    m.overrides = [(0u8, 2u32, 0u16), (5, 5, 0xD), (9, 9, 0xD), (10, 10, 0xD), (11, 11, 0xD)]
        .iter()
        .map(|&(source, gsi, f)| Override { bus: 0, source, gsi, flags: IntiFlags(f) })
        .collect();
    a
}

fn line(gsi: u32, level: bool, active_low: bool) -> Line {
    Line { gsi, level, active_low }
}

#[test]
fn minipc_routes_only_the_sci() {
    let a = minipc_acpi();
    for hv in [false, true] {
        let p = plan(&a, hv);
        assert_eq!(p.routes.len(), 1, "{:?}", p.routes);
        let r = p.routes[0];
        assert_eq!((r.purpose, r.source), (Purpose::Sci, 9));
        assert_eq!(r.line, line(9, true, true), "Mini-PC: GSI 9, level, active low (override flags 0xF)");
        assert_eq!(p.notes, vec![PlanNote::No8042], "the FADT says no 8042, under a hypervisor too");
    }
    let mut n = Vec::new();
    assert_eq!(nmi_pin(a.madt.as_ref(), 0, &mut n), NmiPin { lint: 1, active_low: false });
    assert_eq!(nmi_pin(a.madt.as_ref(), 15, &mut n), NmiPin { lint: 1, active_low: false });
    assert!(n.is_empty());
}

#[test]
fn q35_routes_sci_and_ps2() {
    let a = q35_acpi();
    let p = plan(&a, true);
    assert!(p.notes.is_empty(), "{:?}", p.notes);
    let got: Vec<_> = p.routes.iter().map(|r| (r.purpose, r.line)).collect();
    assert_eq!(
        got,
        vec![(Purpose::Sci, line(9, true, false)), (Purpose::Ps2Keyboard, line(1, false, false)), (Purpose::Ps2Mouse, line(12, false, false))]
    );
}

#[test]
fn isa_lines() {
    let a = minipc_acpi();
    let m = a.madt.as_ref().unwrap();
    let mut n = Vec::new();
    assert_eq!(isa_line(m, 0, false, false, &mut n), line(2, false, false), "IRQ 0 -> GSI 2, bus defaults");
    assert_eq!(isa_line(m, 1, false, false, &mut n), line(1, false, true), "IRQ 1 flags 7: edge, active low");
    assert_eq!(isa_line(m, 4, false, false, &mut n), line(4, false, false), "no override: identity, ISA defaults");
    assert_eq!(isa_line(m, 4, true, true, &mut n), line(4, true, true), "SCI defaults: level, low");
    assert_eq!(isa_line(m, 300, true, true, &mut n), line(300, true, true), "SCI_INT above 255 is a GSI");
    assert!(n.is_empty());
    let mut m2 = m.clone();
    m2.overrides.push(Override { bus: 0, source: 7, gsi: 7, flags: IntiFlags(0b1010) });
    assert_eq!(isa_line(&m2, 7, false, false, &mut n), line(7, false, false), "reserved bits: defaults");
    assert_eq!(n, vec![PlanNote::ReservedFlags { source: 7, flags: 0b1010 }]);
    let mut m3 = m.clone();
    m3.overrides.push(Override { bus: 1, source: 4, gsi: 40, flags: IntiFlags(0xF) });
    assert_eq!(isa_line(&m3, 4, false, false, &mut n), line(4, false, false), "only bus 0 (ISA) overrides count");
}

#[test]
fn broken_one_thing_at_a_time() {
    // No MADT: nothing.
    let mut a = minipc_acpi();
    a.madt = None;
    let p = plan(&a, true);
    assert!(p.routes.is_empty());
    assert_eq!(p.notes, vec![PlanNote::NoMadt]);

    // Hardware-reduced ACPI: no SCI.
    let mut a = minipc_acpi();
    a.fadt.as_mut().unwrap().flags |= crate::fadt::flags::HW_REDUCED_ACPI;
    let p = plan(&a, false);
    assert!(p.routes.is_empty());
    assert!(p.notes.contains(&PlanNote::NoSci));

    // No FADT at all, under QEMU: PS/2 assumed (no boot-arch flags to ask), no SCI.
    let mut a = q35_acpi();
    a.fadt = None;
    let p = plan(&a, true);
    assert_eq!(p.routes.iter().map(|r| r.purpose).collect::<Vec<_>>(), vec![Purpose::Ps2Keyboard, Purpose::Ps2Mouse]);
    let p = plan(&a, false);
    assert!(p.routes.is_empty(), "on hardware PS/2 needs the FADT's word");

    // FADT revision 2 (no boot-arch flags): PS/2 only under a hypervisor.
    let mut a = q35_acpi();
    a.fadt.as_mut().unwrap().revision = 2;
    assert_eq!(plan(&a, true).routes.len(), 3);
    assert_eq!(plan(&a, false).routes.len(), 1);

    // No I/O APIC for GSI 9 (only the second one of the Mini-PC).
    let mut a = minipc_acpi();
    a.madt.as_mut().unwrap().ioapics.remove(0);
    let p = plan(&a, false);
    assert!(p.routes.is_empty());
    assert!(p.notes.contains(&PlanNote::NoIoApic { gsi: 9, purpose: Purpose::Sci }));

    // An override sends the mouse onto the SCI's GSI: the second user is dropped.
    let mut a = q35_acpi();
    a.madt.as_mut().unwrap().overrides.push(Override { bus: 0, source: 12, gsi: 9, flags: IntiFlags(0) });
    let p = plan(&a, true);
    assert_eq!(p.routes.len(), 2);
    assert!(p.notes.contains(&PlanNote::SharedGsi { gsi: 9, purpose: Purpose::Ps2Mouse }));
}

#[test]
fn nmi_pins() {
    let mut m = Madt::default();
    let mut n = Vec::new();
    assert_eq!(nmi_pin(None, 0, &mut n), NmiPin { lint: 1, active_low: false }, "no MADT: LINT1");
    assert_eq!(nmi_pin(Some(&m), 0, &mut n), NmiPin { lint: 1, active_low: false }, "no entry: LINT1");
    m.cpus.push(crate::madt::Cpu { apic_id: 4, uid: 7, enabled: true, online_capable: false, x2apic: false });
    m.lapic_nmis.push(LapicNmi { uid: None, flags: IntiFlags(0), lint: 1 });
    m.lapic_nmis.push(LapicNmi { uid: Some(7), flags: IntiFlags(3), lint: 0 });
    assert_eq!(nmi_pin(Some(&m), 4, &mut n), NmiPin { lint: 0, active_low: true }, "the CPU's own entry wins");
    assert_eq!(nmi_pin(Some(&m), 5, &mut n), NmiPin { lint: 1, active_low: false }, "others take the all-CPUs entry");
    assert!(n.is_empty());
    m.lapic_nmis = vec![LapicNmi { uid: None, flags: IntiFlags(0), lint: 9 }];
    assert_eq!(nmi_pin(Some(&m), 4, &mut n), NmiPin { lint: 1, active_low: false });
    assert_eq!(n, vec![PlanNote::BadNmiLint { lint: 9 }]);
}

#[test]
fn ioapic_entries() {
    let e = Entry::fixed(0x30, &line(9, true, true), 0, false);
    assert_eq!((e.low, e.high), (0xA030, 0), "level | active low | vector, fixed, physical");
    assert!(e.level() && e.active_low() && !e.masked() && e.delivery() == ioapic::DM_FIXED && e.vector() == 0x30);
    let e = Entry::fixed(0x31, &line(1, false, false), 15, true);
    assert_eq!((e.low, e.high, e.dest()), (0x1_0031, 15 << 24, 15));
    assert!(Entry::off().masked() && Entry::off().vector() == 0);
    assert_eq!(Entry { low: ioapic::DM_SMI, high: 0 }.delivery_name(), "smi");
    assert_eq!(pins_from_version(0x0017_0011), Some(24), "Mini-PC I/O APIC 33: GSI 0-23");
    assert_eq!(pins_from_version(0x001F_0021), Some(32), "Mini-PC I/O APIC 34: GSI 24-55");
    assert_eq!(pins_from_version(0xFFFF_FFFF), None, "nothing answers");
    assert_eq!(pins_from_version(0x00FF_0020), Some(256), "the field's largest value, not a limit of ours");
}

#[test]
fn storm_guard_counts_per_window() {
    let g = StormGuard::new(1_000_000, 100);
    let t0 = 5_000_000_000u64;
    for i in 0..100 {
        assert!(!g.hit(t0 + i * 1000), "hit {i} is no storm");
    }
    assert!(g.hit(t0 + 100_000), "the 101st within 1 ms is");
    assert!(!g.hit(t0 + 1_000_000), "a new window starts afresh");
    for i in 1..100 {
        assert!(!g.hit(t0 + 1_000_000 + i));
    }
    g.reset();
    assert!(!g.hit(t0 + 1_000_500));
    // A clock that goes backwards starts a new window instead of counting into an old one.
    assert!(!g.hit(t0 - 10));
    // 100 per ms spread evenly never trips it.
    let g = StormGuard::new(1_000_000, 100);
    assert!((0..10_000u64).all(|i| !g.hit(t0 + i * 10_001)));
}

#[test]
fn storm_guard_without_a_clock_yet() {
    // time::now_ns() is 0 before time::init: a constant clock would make the window a lifetime count.
    let stuck = StormGuard::new(1_000_000, 100);
    assert!((0..100).all(|_| !stuck.hit(0)));
    assert!(stuck.hit(0), "why storm_clock exists: the 101st key ever would mask the keyboard");
    // With the TSC fallback a typing keyboard (one key per 10 ms at 3 GHz) never trips it ...
    let g = StormGuard::new(1_000_000, 100);
    let mut tsc = 7_000_000_000u64;
    for i in 0..10_000 {
        tsc += 30_000_000;
        assert!(!g.hit(storm_clock(0, || tsc)), "key {i}");
    }
    // ... while a stuck level line (an interrupt every ~2000 cycles) still does within its first window.
    let g = StormGuard::new(1_000_000, 100);
    assert!((0..200u64).any(|i| g.hit(storm_clock(0, || tsc + i * 2_000))));
    // Once the clock runs, it is used as it is (the TSC is not read).
    assert_eq!(storm_clock(42, || panic!("TSC read although the clock runs")), 42);
}

/// xorshift64*: deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

#[test]
fn hostile_madts_never_panic() {
    let mut r = Rng(0xC0DE_0003_F00D_0001);
    for _ in 0..5_000 {
        let mut a = q35_acpi();
        let f = a.fadt.as_mut().unwrap();
        f.sci_int = r.next() as u16;
        f.revision = r.next() as u8;
        f.boot_arch = r.next() as u16;
        let m = a.madt.as_mut().unwrap();
        m.overrides = (0..r.next() % 40).map(|_| Override { bus: r.next() as u8 % 2, source: r.next() as u8, gsi: r.next() as u32, flags: IntiFlags(r.next() as u16) }).collect();
        m.ioapics = (0..r.next() % 4).map(|_| IoApic { id: r.next() as u8, address: r.next() as u32, gsi_base: r.next() as u32 % 512 }).collect();
        m.lapic_nmis = (0..r.next() % 4).map(|_| LapicNmi { uid: (r.next() % 2 == 0).then(|| r.next() as u32 % 4), flags: IntiFlags(r.next() as u16), lint: r.next() as u8 }).collect();
        let p = plan(&a, r.next() % 2 == 0);
        assert!(p.routes.len() <= 3);
        let mut gsis: Vec<u32> = p.routes.iter().map(|x| x.line.gsi).collect();
        gsis.sort_unstable();
        gsis.dedup();
        assert_eq!(gsis.len(), p.routes.len(), "never two routes on one GSI");
        let mut n = Vec::new();
        let pin = nmi_pin(a.madt.as_ref(), r.next() as u32 % 20, &mut n);
        assert!(pin.lint <= 1);
    }
}
