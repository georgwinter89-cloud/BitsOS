//! CP-2 ktests, all `Safe` (read-only; they run on the Mini-PC too).
//! - `cp.acpi.synthetic*`: the host tests' Mini-PC world written into frames of our own and read back
//!   through the kernel's reader (direct map, a FADT across a page boundary, holes, bad checksums).
//! - `cp.acpi.reserved_read`: the `hal::mmio` path for firmware-reserved memory (the HPET page as a
//!   stand-in), and the guard keeping a read out of an "ECAM" window.
//! - `cp.acpi.live.*`: the machine's own tables as `acpi::init` read them. Without the kernel hook (desk
//!   change request CP-2/1) they end as `KTEST skip` (and a Warn event), never as a pass.

use crate::guard::{BarRange, PciFn, PciUnreadable, PciView, Refusal};
use crate::kmem::KernelMem;
use crate::mcfg::Ecam;
use crate::mem::{MemError, PhysMem};
use crate::memmap::{MemMap, Region};
use crate::testkit::{dsdt, fix_checksum, minipc_at, off, FakeMem};
use crate::{live, parse, sdt::Gas, Acpi, RootKind};
use alloc::vec;
use alloc::vec::Vec;
use bootinfo::memtype;
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("cp.acpi.synthetic", Safe, synthetic);
ktest!("cp.acpi.synthetic_hostile", Safe, synthetic_hostile);
ktest!("cp.acpi.reserved_read", Safe, reserved_read);
ktest!("cp.acpi.live.tables", Safe, live_tables);
ktest!("cp.acpi.live.madt", Safe, live_madt);
ktest!("cp.acpi.live.guard", Safe, live_guard);
ktest!("cp.acpi.live.summary", Safe, live_summary);

/// A PCI view without devices, so synthetic reads never scan the bus.
struct NoPci;
impl PciView for NoPci {
    fn function(&self, _: u8, _: u8, _: u8) -> Option<PciFn> {
        None
    }
    fn storage_bars(&self) -> Result<Vec<BarRange>, PciUnreadable> {
        Ok(Vec::new())
    }
}

/// 64 KiB of our own frames below 4 GiB (the RSDT holds 32-bit addresses), freed on drop.
struct Frames(u64);
const ORDER: u32 = 4;
const SPAN: u64 = 4096 << ORDER;

impl Frames {
    fn new() -> Result<Frames, alloc::string::String> {
        let fa = mm_core::frames().ok_or("no frame allocator")?;
        fa.alloc(ORDER, mm_core::Zone::Dma32, mm_core::Kind::Other).map(Frames).ok_or_else(|| "no 64 KiB below 4 GiB".into())
    }
    fn write(&self, world: &FakeMem) -> R {
        kassert!(off::SPAN <= SPAN);
        kassert!(hal::dma_buf::write(self.0, SPAN as usize, 0, &vec![0u8; SPAN as usize]));
        for (at, b) in &world.blobs {
            kassert!(hal::dma_buf::write(self.0, SPAN as usize, (at - self.0) as usize, b), "blob at {at:#x}");
        }
        Ok(())
    }
}

impl Drop for Frames {
    fn drop(&mut self) {
        if let Some(fa) = mm_core::frames() {
            fa.free(self.0, ORDER, mm_core::Kind::Other);
        }
    }
}

fn reclaim_map(start: u64, end: u64) -> MemMap {
    MemMap::new(vec![Region { start, end, ty: memtype::ACPI_RECLAIM }])
}

fn reader(map: &MemMap) -> KernelMem<'_> {
    KernelMem::new(map, hal::cpu::facts().phys_addr_bits, &NoPci)
}

fn synthetic() -> R {
    let fr = Frames::new()?;
    let base = fr.0;
    fr.write(&minipc_at(base))?;
    let map = reclaim_map(base, base + SPAN);
    let mut mem = reader(&map);
    let a = parse::parse(&mut mem, base + off::RSDP);
    kassert!(a.notes.is_empty(), "notes {:?}", a.notes);
    kassert!(mem.windows() == 0, "RAM must come through the direct map, not hal::mmio");
    kassert!(a.root == Some(RootKind::Xsdt));
    kassert!(a.tables.len() == 8, "{} tables", a.tables.len());
    let f = a.fadt.as_ref().ok_or("no FADT")?;
    kassert!((base + off::FADT) / 4096 != (base + off::FADT + 275) / 4096, "the FADT must cross a page");
    let reset = f.usable_reset().ok_or("no reset register")?;
    kassert!(reset.reg.address == 0xB2 && reset.value == 0xBE);
    kassert!(f.pm1a_cnt.map(|g| g.address) == Some(0x804));
    let m = a.madt.as_ref().ok_or("no MADT")?;
    kassert!(m.cpus.len() == 16 && m.ioapics.len() == 2);
    kassert!(a.hpet.and_then(|h| h.mmio_base()) == Some(0xFED0_0000));
    kassert!(a.mcfg.first().map(|e| e.base) == Some(0xE000_0000));
    kassert!(a.s5.map(|s| (s.0.typ_a, s.2)) == Some((5, base + off::DSDT)));
    Ok(())
}

fn synthetic_hostile() -> R {
    let fr = Frames::new()?;
    let base = fr.0;
    let mut w = minipc_at(base);
    // A bad checksum; a DSDT without _S5, so the SSDTs are searched; the second SSDT's length runs past the
    // end of the ACPI range into memory the map does not name.
    if let Some(t) = w.get_mut(base + off::HPET) {
        t[40] ^= 1;
    }
    w.put(base + off::DSDT, dsdt(&[0x14, 0x05, b'_', b'P', b'T', b'S', 0x01]));
    if let Some(t) = w.get_mut(base + off::SSDT2) {
        t[4..8].copy_from_slice(&0x8000u32.to_le_bytes());
        fix_checksum(t, 9);
    }
    fr.write(&w)?;
    let map = reclaim_map(base, base + SPAN);
    let a = parse::parse(&mut reader(&map), base + off::RSDP);
    let has = |what: &str, sig: &[u8; 4]| a.notes.iter().any(|n| n.what == what && &n.signature == sig);
    kassert!(a.hpet.is_none() && has("bad_checksum", b"HPET"), "notes {:?}", a.notes);
    kassert!(has("not_table_memory", b"SSDT"), "notes {:?}", a.notes);
    kassert!(a.fadt.is_some() && a.madt.is_some() && a.s5.is_none());
    // The map covering only the first 0x3000 bytes: the FADT starts in RAM and runs into a hole.
    let short = reclaim_map(base, base + 0x3000);
    let mut mem = reader(&short);
    let a = parse::parse(&mut mem, base + off::RSDP);
    kassert!(a.fadt.is_none(), "FADT read across into a hole");
    kassert!(a.notes.iter().any(|n| n.what == "not_table_memory" && &n.signature == b"FACP"), "notes {:?}", a.notes);
    let mut b = [0u8; 8];
    kassert!(mem.read(base + 0x2FFC, &mut b) == Err(MemError::NotTableMemory));
    kassert!(mem.read(u64::MAX - 3, &mut b) == Err(MemError::OutOfRange));
    Ok(())
}

fn reserved_read() -> R {
    // The HPET register page, declared firmware-reserved: read through hal::mmio (UC), never the direct map.
    const PAGE: u64 = 0xFED0_0000;
    let map = MemMap::new(vec![Region { start: PAGE, end: PAGE + 4096, ty: crate::memmap::RESERVED }]);
    let mut mem = reader(&map);
    let mut cap = [0u8; 8];
    mem.read(PAGE, &mut cap).map_err(|e| alloc::format!("read: {e:?}"))?;
    kassert!(mem.windows() == 1);
    let vendor = u16::from_le_bytes([cap[2], cap[3]]);
    kassert!(vendor != 0 && vendor != 0xFFFF, "HPET capabilities {cap:x?}");
    // Inside an ECAM window the guard keeps the reader out.
    let ecam = [Ecam { base: PAGE, segment: 0, bus_start: 0, bus_end: 0 }];
    let mut mem = reader(&map).with_ecam(&ecam);
    kassert!(mem.read(PAGE, &mut cap) == Err(MemError::Guarded));
    // MMIO-typed memory is never read as a table.
    let mmio = MemMap::new(vec![Region { start: PAGE, end: PAGE + 4096, ty: memtype::MMIO }]);
    kassert!(reader(&mmio).read(PAGE, &mut cap) == Err(MemError::NotTableMemory));
    Ok(())
}

/// The live tables, or a `KTEST skip` when the kernel never called `acpi::init` (desk change request
/// CP-2/1): the live checks then report as skipped, never as passed.
fn live() -> Result<&'static Acpi, alloc::string::String> {
    match live::tables() {
        Some(t) => Ok(t),
        None => {
            klog::event!(Warn, "acpi", "ktest.no_init"; "acpi::init was not called by the kernel (desk change request CP-2/1); live checks skipped");
            Err(ktest::skip("acpi::init not called by the kernel (desk change request CP-2/1)").err().unwrap_or_default())
        }
    }
}

fn live_tables() -> R {
    let a = live()?;
    kassert!(a.rsdp.is_some(), "no RSDP");
    kassert!(a.root.is_some(), "no root table, notes {:?}", a.notes);
    kassert!(a.fadt.is_some() && a.madt.is_some(), "FADT/MADT missing");
    // q35 and the Mini-PC both have HPET and MCFG, and an \_S5.
    kassert!(a.hpet.is_some(), "no HPET");
    kassert!(!a.mcfg.is_empty(), "no MCFG");
    kassert!(a.s5.is_some(), "no _S5");
    kassert!(a.dsdt.is_some_and(|d| d.checksum_ok == Some(true)), "DSDT checksum");
    let f = a.fadt.as_ref().ok_or("no FADT")?;
    if !f.hw_reduced() && f.pm1a_cnt.is_some() {
        kassert!(live::sci_en_at_boot().is_some(), "SCI_EN not readable");
    }
    Ok(())
}

fn live_madt() -> R {
    let a = live()?;
    let m = a.madt.as_ref().ok_or("no MADT")?;
    kassert!(m.lapic_address == 0xFEE0_0000, "LAPIC at {:#x}", m.lapic_address);
    kassert!(!m.ioapics.is_empty(), "no I/O APIC");
    let max = hal::cpu::cpuid(0, 0).eax;
    let bsp = if max >= 0xB { hal::cpu::cpuid(0xB, 0).edx } else { hal::cpu::facts().initial_apic_id };
    kassert!(m.cpus.iter().any(|c| c.enabled && c.apic_id == bsp), "BSP APIC ID {bsp} not in the MADT");
    // QEMU and the Mini-PC route ISA IRQ 0 to GSI 2.
    kassert!(m.isa_override(0).map(|o| o.gsi) == Some(2), "IRQ 0 override");
    Ok(())
}

fn live_guard() -> R {
    let a = live()?;
    kassert!(matches!(live::check_io(0x1F0, 1, false), Err(Refusal::LegacyPort(_))));
    kassert!(matches!(live::check_io(0x64, 1, false), Err(Refusal::LegacyPort(_))));
    kassert!(matches!(live::check_io(0xCF8, 4, false), Err(Refusal::LegacyPort(_))));
    // Kernel memory (a heap object) is RAM.
    let x = alloc::boxed::Box::new(0u64);
    let phys = hal::virt_to_phys(&*x as *const u64 as u64).ok_or("heap not in the direct map")?;
    kassert!(live::check_mem(phys, 8) == Err(Refusal::Ram));
    if let Some(e) = a.mcfg.first() {
        kassert!(live::check_mem(e.window().0 + 0x100, 4) == Err(Refusal::Ecam));
    }
    let f = a.fadt.as_ref().ok_or("no FADT")?;
    if let Some(r) = f.usable_reset() {
        kassert!(live::check_gas(&r.reg, 1, true).is_ok(), "reset register {:?} refused", r.reg);
    }
    if let Some(g) = f.pm1a_cnt {
        kassert!(live::check_gas(&g, 2, false).is_ok(), "PM1a control {g:?} refused");
    }
    if let Some(base) = a.hpet.and_then(|h| h.mmio_base()) {
        kassert!(live::check_mem(base + 0x10, 8).is_ok(), "HPET configuration register refused");
    }
    kassert!(live::check_gas(&Gas::io(0x60, 1), 1, false).is_err());
    Ok(())
}

fn live_summary() -> R {
    live()?;
    let s = live::summary();
    kassert!(s.iter().any(|l| l.contains("FADT rev")), "summary {s:?}");
    kassert!(s.iter().any(|l| l.contains("I/O APIC id")), "summary {s:?}");
    for l in &s {
        klog::event!(Info, "acpi", "ktest.summary", line = l.as_str());
    }
    Ok(())
}
