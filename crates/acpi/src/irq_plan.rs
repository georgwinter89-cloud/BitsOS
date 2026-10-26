//! CP-3, pure part: which interrupt lines the I/O APICs carry and how (host-tested with the testkit worlds).
//!
//! Only two kinds of lines are routed (spec/core.md §5.2, V2-PLAN §2.4): the ACPI SCI (FADT `SCI_INT`, level and
//! active low unless an interrupt source override says otherwise; ACPI 6.5 §5.2.9, Linux
//! `acpi_sci_ioapic_setup`) and, where an 8042 exists, the PS/2 keyboard and mouse (ISA IRQ 1 and 12, edge and
//! active high unless overridden). Everything else is MSI/MSI-X from the vector allocator; every other pin stays
//! masked. The LAPIC's NMI pin comes from the MADT's LAPIC NMI entries.

use crate::madt::{IntiFlags, Madt, Polarity, Trigger};
use crate::Acpi;
use alloc::vec::Vec;
use hal::apic::NmiPin;

/// What a routed line is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    Sci,
    Ps2Keyboard,
    Ps2Mouse,
}

impl Purpose {
    pub fn name(self) -> &'static str {
        match self {
            Purpose::Sci => "sci",
            Purpose::Ps2Keyboard => "ps2.keyboard",
            Purpose::Ps2Mouse => "ps2.mouse",
        }
    }
}

/// A global system interrupt with its electrical behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Line {
    pub gsi: u32,
    pub level: bool,
    pub active_low: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Planned {
    pub purpose: Purpose,
    /// The ISA IRQ (or SCI_INT) the line was asked for.
    pub source: u32,
    pub line: Line,
}

/// Why something was not planned the normal way; each becomes a Warn/Info event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanNote {
    /// No MADT: no I/O APIC data, nothing is routed.
    NoMadt,
    /// No FADT or hardware-reduced ACPI: no SCI to route.
    NoSci,
    /// No 8042 according to the FADT (the Mini-PC): PS/2 not routed.
    No8042,
    /// An override with reserved polarity/trigger bits: the default was used.
    ReservedFlags { source: u32, flags: u16 },
    /// Two purposes want the same GSI; the later one is dropped.
    SharedGsi { gsi: u32, purpose: Purpose },
    /// No I/O APIC serves this GSI.
    NoIoApic { gsi: u32, purpose: Purpose },
    /// The MADT's LAPIC NMI entry names a LINT other than 0/1: the default (LINT1) is used.
    BadNmiLint { lint: u8 },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub routes: Vec<Planned>,
    pub notes: Vec<PlanNote>,
}

/// An ISA IRQ (or the SCI number) as a GSI: an interrupt source override on bus 0 wins, its "bus default"
/// fields fall back to `default_level`/`default_low`; without one the GSI is the IRQ itself.
pub fn isa_line(madt: &Madt, source: u32, default_level: bool, default_low: bool, notes: &mut Vec<PlanNote>) -> Line {
    let ov = u8::try_from(source).ok().and_then(|irq| madt.isa_override(irq));
    let Some(o) = ov else {
        return Line { gsi: source, level: default_level, active_low: default_low };
    };
    let f: IntiFlags = o.flags;
    if matches!(f.polarity(), Polarity::Reserved) || matches!(f.trigger(), Trigger::Reserved) {
        notes.push(PlanNote::ReservedFlags { source, flags: f.0 });
    }
    let active_low = match f.polarity() {
        Polarity::High => false,
        Polarity::Low => true,
        Polarity::BusDefault | Polarity::Reserved => default_low,
    };
    let level = match f.trigger() {
        Trigger::Edge => false,
        Trigger::Level => true,
        Trigger::BusDefault | Trigger::Reserved => default_level,
    };
    Line { gsi: o.gsi, level, active_low }
}

/// The routes for this machine. `hypervisor`: under QEMU an FADT before revision 3 (no boot-arch flags) is
/// taken to have an 8042; on hardware only an explicit "8042 present" routes PS/2.
pub fn plan(acpi: &Acpi, hypervisor: bool) -> Plan {
    let mut p = Plan::default();
    let Some(madt) = acpi.madt.as_ref() else {
        p.notes.push(PlanNote::NoMadt);
        return p;
    };
    let mut want: Vec<(Purpose, u32, bool, bool)> = Vec::new();
    match acpi.fadt.as_ref().filter(|f| !f.hw_reduced()) {
        Some(f) => want.push((Purpose::Sci, f.sci_int as u32, true, true)),
        None => p.notes.push(PlanNote::NoSci),
    }
    let ps2 = match acpi.fadt.as_ref().and_then(|f| f.has_8042()) {
        Some(v) => v,
        None => hypervisor,
    };
    if ps2 {
        want.push((Purpose::Ps2Keyboard, 1, false, false));
        want.push((Purpose::Ps2Mouse, 12, false, false));
    } else {
        p.notes.push(PlanNote::No8042);
    }
    for (purpose, source, level, low) in want {
        let line = isa_line(madt, source, level, low, &mut p.notes);
        if p.routes.iter().any(|r| r.line.gsi == line.gsi) {
            p.notes.push(PlanNote::SharedGsi { gsi: line.gsi, purpose });
            continue;
        }
        if madt.ioapic_for(line.gsi).is_none() {
            p.notes.push(PlanNote::NoIoApic { gsi: line.gsi, purpose });
            continue;
        }
        p.routes.push(Planned { purpose, source, line });
    }
    p
}

/// The NMI pin of the CPU with APIC ID `apic_id`: the MADT's LAPIC NMI entry for its processor UID or for
/// all processors; LINT1 active high when there is none (Linux's default for the BSP).
pub fn nmi_pin(madt: Option<&Madt>, apic_id: u32, notes: &mut Vec<PlanNote>) -> NmiPin {
    let default = NmiPin { lint: 1, active_low: false };
    let Some(m) = madt else { return default };
    let uid = m.cpus.iter().find(|c| c.apic_id == apic_id).map(|c| c.uid);
    let entry = m.lapic_nmis.iter().find(|n| n.uid.is_some() && n.uid == uid).or_else(|| m.lapic_nmis.iter().find(|n| n.uid.is_none()));
    let Some(e) = entry else { return default };
    if e.lint > 1 {
        notes.push(PlanNote::BadNmiLint { lint: e.lint });
        return default;
    }
    NmiPin { lint: e.lint, active_low: matches!(e.flags.polarity(), Polarity::Low) }
}

/// The storm guard's clock: `time::now_ns()`, or, while that is still 0 (before `time::init`), the raw TSC
/// read as nanoseconds. Every x86-64-v3 CPU's TSC runs at 1 GHz or more, so a window of N ticks is at most N ns
/// long: the fallback only ever judges more leniently on rate, never masks a line that would pass with the
/// real clock. Without it a stuck clock (0) would turn the window into a lifetime count and mask a keyboard
/// after its 101st key.
pub fn storm_clock(now_ns: u64, tsc: impl FnOnce() -> u64) -> u64 {
    if now_ns != 0 { now_ns } else { tsc() }
}

/// Interrupt storm guard (V1 `apic/msi.asm`: more than 100 interrupts in a millisecond -> the line is masked and
/// its owner polls). Counts per window; `hit` returns true once the count passes `limit` inside one window.
/// Lock-free; one line is delivered to one CPU, so relaxed atomics suffice.
pub struct StormGuard {
    window_ns: u64,
    limit: u32,
    start: core::sync::atomic::AtomicU64,
    count: core::sync::atomic::AtomicU32,
}

impl StormGuard {
    pub const fn new(window_ns: u64, limit: u32) -> Self {
        StormGuard { window_ns, limit, start: core::sync::atomic::AtomicU64::new(0), count: core::sync::atomic::AtomicU32::new(0) }
    }

    /// One interrupt at `now_ns`; true when this one makes it a storm.
    pub fn hit(&self, now_ns: u64) -> bool {
        use core::sync::atomic::Ordering::Relaxed;
        let start = self.start.load(Relaxed);
        if now_ns.wrapping_sub(start) >= self.window_ns || now_ns < start {
            self.start.store(now_ns, Relaxed);
            self.count.store(1, Relaxed);
            return 1 > self.limit;
        }
        self.count.fetch_add(1, Relaxed) + 1 > self.limit
    }

    /// Starts counting afresh (after the owner unmasked the line).
    pub fn reset(&self) {
        use core::sync::atomic::Ordering::Relaxed;
        self.start.store(0, Relaxed);
        self.count.store(0, Relaxed);
    }
}
