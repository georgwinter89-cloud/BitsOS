//! I/O APIC driver (CP-3; Intel 82093AA datasheet, Linux `arch/x86/kernel/apic/io_apic.c`). Safe code: the two
//! registers (IOREGSEL at 0x00, IOWIN at 0x10) are reached through a `hal::mmio::IoMem` window, the select/data
//! pair under an `IrqSpinLock` (an IRQ handler may mask its own pin).
//!
//! `mask_all` masks every pin except those the firmware gave to SMI (left alone, as V1 and Linux do) and clears
//! a stale Remote IRR on level pins by switching them to edge and back (Linux `clear_IO_APIC_pin`).

use crate::irq_plan::Line;
use hal::mmio::{IoMem, MmioError};
use ksync::IrqSpinLock;

const IOREGSEL: usize = 0x00;
const IOWIN: usize = 0x10;
/// Bytes of the register window.
pub const WINDOW: usize = 0x20;

pub const REG_ID: u32 = 0x00;
pub const REG_VERSION: u32 = 0x01;
pub const REG_REDTBL: u32 = 0x10;

pub const MASKED: u32 = 1 << 16;
pub const LEVEL: u32 = 1 << 15;
pub const REMOTE_IRR: u32 = 1 << 14;
pub const ACTIVE_LOW: u32 = 1 << 13;
pub const DELIVERY_PENDING: u32 = 1 << 12;
pub const DEST_LOGICAL: u32 = 1 << 11;
pub const DM_MASK: u32 = 0x700;
pub const DM_FIXED: u32 = 0x000;
pub const DM_SMI: u32 = 0x200;
pub const DM_NMI: u32 = 0x400;
pub const DM_EXTINT: u32 = 0x700;

/// One redirection table entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub low: u32,
    pub high: u32,
}

impl Entry {
    /// Fixed delivery of `vector` to the CPU with APIC ID `dest` (physical mode; 8-bit destination field).
    pub fn fixed(vector: u8, line: &Line, dest: u8, masked: bool) -> Entry {
        let low = DM_FIXED
            | vector as u32
            | if line.level { LEVEL } else { 0 }
            | if line.active_low { ACTIVE_LOW } else { 0 }
            | if masked { MASKED } else { 0 };
        Entry { low, high: (dest as u32) << 24 }
    }
    /// The entry `mask_all` leaves on an unused pin.
    pub const fn off() -> Entry {
        Entry { low: MASKED, high: 0 }
    }
    pub fn vector(&self) -> u8 {
        self.low as u8
    }
    pub fn masked(&self) -> bool {
        self.low & MASKED != 0
    }
    pub fn level(&self) -> bool {
        self.low & LEVEL != 0
    }
    pub fn active_low(&self) -> bool {
        self.low & ACTIVE_LOW != 0
    }
    pub fn remote_irr(&self) -> bool {
        self.low & REMOTE_IRR != 0
    }
    pub fn delivery(&self) -> u32 {
        self.low & DM_MASK
    }
    pub fn dest(&self) -> u8 {
        (self.high >> 24) as u8
    }
    pub fn delivery_name(&self) -> &'static str {
        match self.delivery() {
            DM_FIXED => "fixed",
            0x100 => "lowest",
            DM_SMI => "smi",
            DM_NMI => "nmi",
            0x500 => "init",
            DM_EXTINT => "extint",
            _ => "reserved",
        }
    }
}

/// Pins from the version register (bits 16-23 = highest entry index), `None` when the register reads like
/// nothing answers (all ones).
pub fn pins_from_version(v: u32) -> Option<u32> {
    (v != u32::MAX).then_some(((v >> 16) & 0xFF) + 1)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicError {
    Map(MmioError),
    /// The version register reads all ones: no I/O APIC at that address.
    Absent,
}

/// What `mask_all` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaskReport {
    pub masked: u32,
    /// Pins with SMI delivery the firmware owns; not touched.
    pub smi_left: u32,
    /// Level pins whose Remote IRR was set and was cleared (or tried).
    pub remote_irr_cleared: u32,
    /// Pins the firmware had left unmasked (before masking).
    pub were_unmasked: u32,
}

pub struct IoApic {
    /// From the MADT.
    pub id: u8,
    /// The ID register's own value (bits 24-27); can differ from the MADT's.
    pub hw_id: u8,
    pub phys: u64,
    pub gsi_base: u32,
    pub pins: u32,
    pub version: u8,
    regs: IrqSpinLock<IoMem>,
}

impl IoApic {
    /// Maps the window and reads the version. The caller ran the register guard on `phys` first.
    pub fn open(phys: u64, id: u8, gsi_base: u32) -> Result<IoApic, IoApicError> {
        let w = hal::mmio::map_device(phys, WINDOW, hal::paging::CacheType::Uncached).map_err(IoApicError::Map)?;
        let rd = |reg: u32| -> u32 {
            if w.write32(IOREGSEL, reg).is_err() {
                return u32::MAX;
            }
            w.read32(IOWIN).unwrap_or(u32::MAX)
        };
        let ver = rd(REG_VERSION);
        let pins = pins_from_version(ver).ok_or(IoApicError::Absent)?;
        let hw_id = ((rd(REG_ID) >> 24) & 0xF) as u8;
        Ok(IoApic { id, hw_id, phys, gsi_base, pins, version: ver as u8, regs: IrqSpinLock::named("acpi.ioapic", w) })
    }

    fn read(w: &IoMem, reg: u32) -> u32 {
        if w.write32(IOREGSEL, reg).is_err() {
            return u32::MAX;
        }
        w.read32(IOWIN).unwrap_or(u32::MAX)
    }

    fn write(w: &IoMem, reg: u32, v: u32) {
        if w.write32(IOREGSEL, reg).is_ok() {
            let _ = w.write32(IOWIN, v);
        }
    }

    /// Whether this I/O APIC serves `gsi`, and on which pin.
    pub fn pin_of(&self, gsi: u32) -> Option<u32> {
        let p = gsi.checked_sub(self.gsi_base)?;
        (p < self.pins).then_some(p)
    }

    pub fn entry(&self, pin: u32) -> Option<Entry> {
        if pin >= self.pins {
            return None;
        }
        let w = self.regs.lock();
        Some(Entry { low: Self::read(&w, REG_REDTBL + 2 * pin), high: Self::read(&w, REG_REDTBL + 2 * pin + 1) })
    }

    /// Writes an entry: masked low word first, then the destination, then the low word as asked, so the pin
    /// never fires half-programmed.
    pub fn set_entry(&self, pin: u32, e: Entry) -> bool {
        if pin >= self.pins {
            return false;
        }
        let w = self.regs.lock();
        Self::write(&w, REG_REDTBL + 2 * pin, e.low | MASKED);
        Self::write(&w, REG_REDTBL + 2 * pin + 1, e.high);
        Self::write(&w, REG_REDTBL + 2 * pin, e.low);
        true
    }

    /// Sets or clears the mask bit of `pin` (IRQ-safe: used by the storm guard inside a handler).
    pub fn set_masked(&self, pin: u32, masked: bool) -> bool {
        if pin >= self.pins {
            return false;
        }
        let w = self.regs.lock();
        let low = Self::read(&w, REG_REDTBL + 2 * pin);
        Self::write(&w, REG_REDTBL + 2 * pin, if masked { low | MASKED } else { low & !MASKED });
        true
    }

    /// Masks every pin except SMI ones; clears stale Remote IRR bits.
    pub fn mask_all(&self) -> MaskReport {
        let mut r = MaskReport::default();
        for pin in 0..self.pins {
            let Some(e) = self.entry(pin) else { continue };
            if e.delivery() == DM_SMI {
                r.smi_left += 1;
                continue;
            }
            if !e.masked() {
                r.were_unmasked += 1;
            }
            if e.level() && e.remote_irr() {
                // Edge, then level again: older I/O APICs clear Remote IRR on the trigger change.
                self.set_entry(pin, Entry { low: (e.low & !LEVEL) | MASKED, high: e.high });
                self.set_entry(pin, Entry { low: e.low | MASKED, high: e.high });
                r.remote_irr_cleared += 1;
            }
            self.set_entry(pin, Entry::off());
            r.masked += 1;
        }
        r
    }
}
