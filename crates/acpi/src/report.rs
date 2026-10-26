//! What the tables say, as B6 events at boot and as text lines for `hw_info` (Mini-PC check list values:
//! FADT reset 0xB2 <- 0xBE, two I/O APICs, MCFG 0xE0000000).

use crate::bytes::text;
use crate::fadt::Fadt;
use crate::sdt::Gas;
use crate::{Acpi, RootKind};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use klog::Hex;

fn gas(g: &Option<Gas>) -> String {
    match g {
        Some(g) => format!("{}:{:#x}/{}", g.space.name(), g.address, g.bit_width),
        None => String::from("none"),
    }
}

fn root_name(r: Option<RootKind>) -> &'static str {
    match r {
        Some(RootKind::Xsdt) => "XSDT",
        Some(RootKind::Rsdt) => "RSDT",
        None => "none",
    }
}

/// Signatures in table order, SSDTs folded into a count ("FACP APIC HPET ... SSDTx24").
fn table_list(a: &Acpi) -> String {
    let mut s = String::new();
    for t in a.tables.iter().filter(|t| &t.header.signature != b"SSDT") {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&text(&t.header.signature));
    }
    let n = a.count(b"SSDT");
    if n > 0 {
        s.push_str(&format!(" SSDTx{n}"));
    }
    s
}

fn tristate(v: Option<bool>) -> &'static str {
    match v {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn reset_text(f: &Fadt) -> String {
    match f.reset {
        Some(r) => format!("{}:{:#x}<-{:#x}{}", r.reg.space.name(), r.reg.address, r.value, if f.usable_reset().is_some() { "" } else { " (flag off)" }),
        None => String::from("none"),
    }
}

/// Text lines for `hw_info` (firmware section).
pub fn summary(a: &Acpi, sci_en: Option<bool>) -> Vec<String> {
    let mut v = Vec::new();
    match &a.rsdp {
        Some(r) => v.push(format!("acpi: RSDP rev {} OEM {} at {:#x}, root {}, {} tables: {}", r.revision, text(&r.oem_id), r.phys, root_name(a.root), a.tables.len(), table_list(a))),
        None => v.push(String::from("acpi: no RSDP")),
    }
    if let Some(f) = &a.fadt {
        v.push(format!(
            "acpi: FADT rev {}.{} len {}, reset {}, pm1a_cnt {}, pm1b_cnt {}, pm_tmr {}, sci_int {}, smi_cmd {:#x}, acpi_enable {:#x}, SCI_EN at boot {}",
            f.revision, f.minor_revision, f.length, reset_text(f), gas(&f.pm1a_cnt), gas(&f.pm1b_cnt), gas(&f.pm_tmr), f.sci_int, f.smi_cmd, f.acpi_enable, tristate(sci_en)
        ));
        v.push(format!(
            "acpi: FADT flags {:#x} (hw_reduced {}, low_power_s0 {}), boot_arch {:#x} (8042 {}, legacy {}, no_cmos_rtc {}), gpe0 {} len {}",
            f.flags, f.hw_reduced(), f.low_power_s0(), f.boot_arch, tristate(f.has_8042()), tristate(f.legacy_devices()), f.no_cmos_rtc(), gas(&f.gpe0), f.gpe0_len
        ));
    }
    if let Some(m) = &a.madt {
        v.push(format!("acpi: MADT lapic {:#x}, cpus {} ({} enabled, {} x2apic entries), 8259 {}", m.lapic_address, m.cpus.len(), m.enabled_cpus(), m.cpus.iter().filter(|c| c.x2apic).count(), m.has_8259()));
        for io in &m.ioapics {
            v.push(format!("acpi: I/O APIC id {} at {:#x}, gsi base {}", io.id, io.address, io.gsi_base));
        }
        for o in &m.overrides {
            v.push(format!("acpi: override bus {} irq {} -> gsi {} flags {:#x}", o.bus, o.source, o.gsi, o.flags.0));
        }
        for n in &m.lapic_nmis {
            v.push(format!("acpi: lapic nmi uid {} lint{} flags {:#x}", n.uid.map(|u| format!("{u}")).unwrap_or_else(|| String::from("all")), n.lint, n.flags.0));
        }
        for n in &m.nmi_sources {
            v.push(format!("acpi: nmi source gsi {} flags {:#x}", n.gsi, n.flags.0));
        }
    }
    if let Some(h) = &a.hpet {
        v.push(format!("acpi: HPET at {:#x}, vendor {:#06x}, {} comparators, {}-bit counter, min tick {}", h.base.address, h.vendor(), h.comparators(), if h.counter_64bit() { 64 } else { 32 }, h.min_tick));
    }
    for e in &a.mcfg {
        v.push(format!("acpi: MCFG ECAM {:#x} segment {} buses {}-{}", e.base, e.segment, e.bus_start, e.bus_end));
    }
    if let Some(r) = &a.ivrs {
        let bases: Vec<String> = r.iommus.iter().map(|u| format!("{:#x}@{:02x}:{:02x}.{}", u.base, u.device_id >> 8, (u.device_id >> 3) & 31, u.device_id & 7)).collect();
        v.push(format!("acpi: IVRS iommus {} [{}]", r.iommus.len(), bases.join(" ")));
    }
    if let Some(s) = &a.srat {
        v.push(format!("acpi: SRAT domains {}, cpu entries {}, memory entries {}", s.domains(), s.cpus.len(), s.memory.len()));
    }
    if let Some(d) = &a.dsdt {
        v.push(format!("acpi: DSDT at {:#x} len {} checksum {}", d.phys, d.header.length, tristate(d.checksum_ok)));
    }
    match &a.s5 {
        Some((s, sig, p)) => v.push(format!("acpi: _S5 typ_a {} typ_b {} (in {} at {:#x})", s.typ_a, s.typ_b, text(sig), p)),
        None => v.push(String::from("acpi: _S5 not found")),
    }
    if !a.notes.is_empty() {
        v.push(format!("acpi: {} problems of {} kinds (see acpi table.problem events)", a.notes.total(), a.notes.len()));
    }
    v
}

/// The boot events (B6): facts at Info, every kind of problem at Warn once, with how often it was seen.
pub fn log(a: &Acpi, sci_en: Option<bool>) {
    for n in a.notes.iter() {
        klog::event!(Warn, "acpi", "table.problem", sig = text(&n.signature), phys = Hex(n.phys), what = n.what, field = n.field, detail = Hex(n.detail), count = n.count);
    }
    let Some(r) = &a.rsdp else {
        return;
    };
    klog::event!(Info, "acpi", "tables", rsdp = Hex(r.phys), revision = r.revision, oem = text(&r.oem_id), root = root_name(a.root), count = a.tables.len(), list = table_list(a), problems = a.notes.total());
    if let Some(f) = &a.fadt {
        let rs = f.reset;
        klog::event!(Info, "acpi", "fadt", revision = f.revision, reset = reset_text(f), reset_space = rs.map(|r| r.reg.space.name()).unwrap_or("none"), reset_addr = Hex(rs.map(|r| r.reg.address).unwrap_or(0)), reset_value = Hex(rs.map(|r| r.value as u64).unwrap_or(0)),
            pm1a_cnt = gas(&f.pm1a_cnt), pm1b_cnt = gas(&f.pm1b_cnt), pm_tmr = gas(&f.pm_tmr), smi_cmd = Hex(f.smi_cmd as u64), acpi_enable = Hex(f.acpi_enable as u64),
            sci_en = tristate(sci_en), sci_int = f.sci_int, flags = Hex(f.flags as u64), low_power_s0 = f.low_power_s0(), hw_reduced = f.hw_reduced(), i8042 = tristate(f.has_8042()), legacy = tristate(f.legacy_devices()));
    } else {
        klog::event!(Warn, "acpi", "fadt.absent"; "no usable FADT: reboot falls back to 0xCF9 and the triple fault, no power-off");
    }
    if let Some(m) = &a.madt {
        klog::event!(Info, "acpi", "madt", lapic = Hex(m.lapic_address), cpus = m.cpus.len(), enabled = m.enabled_cpus(), ioapics = m.ioapics.len(), overrides = m.overrides.len(), lapic_nmis = m.lapic_nmis.len(), pic8259 = m.has_8259());
        for io in &m.ioapics {
            klog::event!(Info, "acpi", "madt.ioapic", id = io.id, addr = Hex(io.address as u64), gsi_base = io.gsi_base);
        }
        for o in &m.overrides {
            klog::event!(Info, "acpi", "madt.override", bus = o.bus, irq = o.source, gsi = o.gsi, flags = Hex(o.flags.0 as u64));
        }
    } else {
        klog::event!(Warn, "acpi", "madt.absent"; "no usable MADT: interrupt routing and CPU list unknown");
    }
    if let Some(h) = &a.hpet {
        klog::event!(Info, "acpi", "hpet", addr = Hex(h.base.address), comparators = h.comparators(), counter_bits = if h.counter_64bit() { 64u32 } else { 32 }, min_tick = h.min_tick);
    }
    for e in &a.mcfg {
        klog::event!(Info, "acpi", "mcfg", base = Hex(e.base), segment = e.segment, bus_start = e.bus_start, bus_end = e.bus_end);
    }
    if let Some(r) = &a.ivrs {
        klog::event!(Info, "acpi", "ivrs", iommus = r.iommus.len(), first_base = Hex(r.iommus.first().map(|u| u.base).unwrap_or(0)));
    }
    if let Some(s) = &a.srat {
        klog::event!(Info, "acpi", "srat", domains = s.domains(), cpus = s.cpus.len(), memory = s.memory.len());
    }
    match &a.s5 {
        Some((s, sig, p)) => klog::event!(Info, "acpi", "s5", typ_a = s.typ_a, typ_b = s.typ_b, table = text(sig), phys = Hex(*p)),
        None => klog::event!(Warn, "acpi", "s5.absent"; "no Name(_S5, Package) found: power-off has no SLP_TYP"),
    }
}
