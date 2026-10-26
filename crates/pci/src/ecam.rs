//! ECAM (PCIe enhanced configuration access, PCI Firmware 3.2 §4.1.2): each bus of a segment is a 1 MiB
//! window at `base + (bus << 20)`, a function's 4 KiB at `dev << 15 | func << 12` inside it. The windows
//! come from CP-2's MCFG (`acpi::tables().mcfg`), every segment and bus range of it.
//!
//! A bus window is mapped (uncached, `hal::mmio`) the first time the walk reaches that bus, so only buses
//! that exist cost a mapping. `hal::mmio::map_device` refuses a window over RAM (a hostile MCFG cannot point
//! the configuration space into the kernel's memory); such a bus reads as absent and is logged once.

use crate::access::{ConfigAccess, Loc, EXT_SPACE};
use acpi::mcfg::Ecam;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use hal::mmio::IoMem;
use ksync::SpinLock;

const BUS_BYTES: u64 = 1 << 20;

pub struct EcamSpace {
    windows: Vec<Ecam>,
    /// Mapped bus windows (G0's vmap never unmaps, so a mapping lives as long as the kernel); `None` = the
    /// mapping was refused, do not try again.
    buses: SpinLock<BTreeMap<(u16, u8), Option<&'static IoMem>>>,
}

impl EcamSpace {
    /// Windows that overlap an earlier one of the same segment are dropped (first wins, as the guard reads
    /// MCFG); the caller logs what it was given.
    pub fn new(given: &[Ecam]) -> EcamSpace {
        let mut windows: Vec<Ecam> = Vec::new();
        for w in given {
            let clash = windows.iter().any(|o| o.segment == w.segment && o.bus_start <= w.bus_end && w.bus_start <= o.bus_end);
            if !clash && w.bus_start <= w.bus_end {
                windows.push(*w);
            }
        }
        EcamSpace { windows, buses: SpinLock::named("pci.ecam", BTreeMap::new()) }
    }

    pub fn windows(&self) -> &[Ecam] {
        &self.windows
    }

    fn window(&self, segment: u16, bus: u8) -> Option<&Ecam> {
        self.windows.iter().find(|w| w.segment == segment && (w.bus_start..=w.bus_end).contains(&bus))
    }

    /// The bus window, mapped on first use. Logs a refused mapping once (outside the lock).
    fn bus(&self, segment: u16, bus: u8) -> Option<&'static IoMem> {
        let w = *self.window(segment, bus)?;
        let mut map = self.buses.lock();
        if let Some(m) = map.get(&(segment, bus)) {
            return *m;
        }
        let phys = w.base.checked_add((bus as u64) << 20)?;
        let r = hal::mmio::map_device(phys, BUS_BYTES as usize, hal::paging::CacheType::Uncached);
        let m: Option<&'static IoMem> = r.ok().map(|io| &*Box::leak(Box::new(io)));
        map.insert((segment, bus), m);
        drop(map);
        if m.is_none() {
            klog::event!(Warn, "pci", "ecam.map_failed", segment = segment, bus = bus, phys = klog::Hex(phys);
                "this bus's configuration window could not be mapped; its functions read as absent");
        }
        m
    }

    /// The bus window for the quiesce path: never maps (that allocates and may log) and only tries the
    /// `pci.ecam` lock until `give_up()`. `None`: gave up; `Some(None)`: no window or not mapped (every bus a
    /// driver uses was mapped long before, at the scan).
    fn bus_bounded(&self, segment: u16, bus: u8, give_up: &dyn Fn() -> bool) -> Option<Option<&'static IoMem>> {
        if self.window(segment, bus).is_none() {
            return Some(None);
        }
        loop {
            if let Some(map) = self.buses.try_lock() {
                return Some(map.get(&(segment, bus)).copied().flatten());
            }
            if give_up() {
                return None;
            }
            core::hint::spin_loop();
        }
    }

    fn reg(at: Loc, off: u16) -> Option<usize> {
        (at.dev < 32 && at.func < 8 && off < EXT_SPACE).then_some(((at.dev as usize) << 15) | ((at.func as usize) << 12) | (off as usize & !3))
    }
}

impl ConfigAccess for EcamSpace {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        let (Some(r), Some(m)) = (Self::reg(at, off), self.bus(at.segment, at.bus)) else { return u32::MAX };
        m.read32(r).unwrap_or(u32::MAX)
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        let (Some(r), Some(m)) = (Self::reg(at, off), self.bus(at.segment, at.bus)) else { return false };
        m.write32(r, v).is_ok()
    }
    fn read32_bounded(&self, at: Loc, off: u16, give_up: &dyn Fn() -> bool) -> Option<u32> {
        let Some(r) = Self::reg(at, off) else { return Some(u32::MAX) };
        Some(self.bus_bounded(at.segment, at.bus, give_up)?.and_then(|m| m.read32(r).ok()).unwrap_or(u32::MAX))
    }
    fn write32_bounded(&self, at: Loc, off: u16, v: u32, give_up: &dyn Fn() -> bool) -> Option<bool> {
        let Some(r) = Self::reg(at, off) else { return Some(false) };
        Some(self.bus_bounded(at.segment, at.bus, give_up)?.is_some_and(|m| m.write32(r, v).is_ok()))
    }
    fn write_narrow(&self, at: Loc, off: u16, width: u8, v: u32) -> bool {
        let (Some(r), Some(m)) = (Self::reg(at, off), self.bus(at.segment, at.bus)) else { return false };
        let r = r | (off as usize & 3);
        match width {
            1 => m.write8(r, v as u8).is_ok(),
            2 => m.write16(r, v as u16).is_ok(),
            _ => false,
        }
    }
    fn narrow_writes(&self) -> bool {
        true
    }
    fn space_size(&self, segment: u16, bus: u8) -> u16 {
        if self.window(segment, bus).is_some() { EXT_SPACE } else { 0 }
    }
    fn name(&self) -> &'static str {
        "ecam"
    }
}
