//! Configuration space access: where a function's registers are and how they are reached.
//!
//! Everything above this module (enumeration, capability walks, BAR sizing) works over `ConfigAccess`, so the
//! host tests drive it with a fake configuration space and the kernel with ECAM (`ecam.rs`) or, when the
//! firmware gives no MCFG, the legacy 0xCF8/0xCFC pair (`PortCf8`).
//!
//! The lock against concurrent users (`CFG_LOCK` in lib.rs) is the caller's: a back end only moves dwords.

/// One function's place: PCI segment (domain), bus, device, function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Loc {
    pub segment: u16,
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
}

impl Loc {
    pub const fn new(segment: u16, bus: u8, dev: u8, func: u8) -> Loc {
        Loc { segment, bus, dev, func }
    }
}

impl core::fmt::Display for Loc {
    /// `bb:dd.f` on segment 0 (as HARDWARE-MINIPC and V1 write it), `ssss:bb:dd.f` elsewhere (lspci).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.segment != 0 {
            write!(f, "{:04x}:", self.segment)?;
        }
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.dev, self.func)
    }
}

/// Size of the standard (PCI 3.0) configuration space.
pub const STD_SPACE: u16 = 0x100;
/// Size of the PCIe extended configuration space.
pub const EXT_SPACE: u16 = 0x1000;

pub trait ConfigAccess: Sync {
    /// The dword at `off & !3`. All ones when nothing answers or `off` is out of this back end's reach.
    fn read32(&self, at: Loc, off: u16) -> u32;
    /// Writes the dword at `off & !3`; false (nothing written) when out of reach.
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool;
    /// Bytes of configuration space this back end reaches on that bus: 4096 (ECAM), 256 (0xCF8), 0 (none).
    fn space_size(&self, segment: u16, bus: u8) -> u16;
    /// Short name for events and hw_info ("ecam", "cf8", "fake").
    fn name(&self) -> &'static str;

    /// Read-modify-write of the dword at `off`: `f` gets the current value and returns the value to write
    /// (`None`: write nothing). Returns the value read. The kernel's locked wrapper holds `pci.cfg` across
    /// both halves, so two enables of functions below one bridge cannot lose each other's bits.
    fn modify32(&self, at: Loc, off: u16, f: &mut dyn FnMut(u32) -> Option<u32>) -> u32 {
        let old = self.read32(at, off);
        if let Some(new) = f(old) {
            self.write32(at, off, new);
        }
        old
    }

    /// PC-5: a byte or word write (`width` 1 or 2) at `off`, leaving the rest of the dword alone (a
    /// read-modify-write would write status bits back as 1 and clear them). False: the back end cannot (0xCF8
    /// moves dwords; the default) or `off` is out of reach.
    fn write_narrow(&self, _at: Loc, _off: u16, _width: u8, _v: u32) -> bool {
        false
    }
    /// Whether `write_narrow` works on this back end.
    fn narrow_writes(&self) -> bool {
        false
    }

    /// For the quiesce path: like `read32`, but a back end with its own lock only tries it until
    /// `give_up()` says so and then returns `None`; it never allocates or maps. Default: `read32`.
    fn read32_bounded(&self, at: Loc, off: u16, _give_up: &dyn Fn() -> bool) -> Option<u32> {
        Some(self.read32(at, off))
    }
    /// The write counterpart of `read32_bounded` (`None`: gave up, nothing written).
    fn write32_bounded(&self, at: Loc, off: u16, v: u32, _give_up: &dyn Fn() -> bool) -> Option<bool> {
        Some(self.write32(at, off, v))
    }
}

/// Read-modify-write of the command register (offset 4, low half). The status half is written 0: its bits
/// are write-1-to-clear. `f` gets the command (0xFFFF when nothing answers) and returns the new value or
/// `None`. Returns the command read.
pub fn modify_command(c: &dyn ConfigAccess, at: Loc, mut f: impl FnMut(u16) -> Option<u16>) -> u16 {
    c.modify32(at, 4, &mut |d| f(d as u16).map(u32::from)) as u16
}

pub fn read16(c: &dyn ConfigAccess, at: Loc, off: u16) -> u16 {
    (c.read32(at, off & !3) >> ((off & 2) * 8)) as u16
}

pub fn read8(c: &dyn ConfigAccess, at: Loc, off: u16) -> u8 {
    (c.read32(at, off & !3) >> ((off & 3) * 8)) as u8
}

/// Vendor 0xFFFF (nothing answers) and 0x0000 (the Mini-PC's ghost functions 00:03.2-00:03.4, spec/core.md
/// §11 trap 3) both mean "no function here".
pub fn vendor_present(vendor: u16) -> bool {
    vendor != 0xFFFF && vendor != 0x0000
}

/// Legacy configuration mechanism #1 (0xCF8/0xCFC): segment 0, the first 256 bytes only. The fallback when
/// the firmware has no MCFG (an event says so), and what the kernel uses before `scan`. hal keeps the
/// address/data pair together; writing needs hal's single `PciConfigWriter`, which this crate holds.
pub struct PortCf8;

static CF8_WRITER: ksync::Once<Option<hal::portio::PciConfigWriter>> = ksync::Once::new();

/// Claims hal's 0xCF8 writer (once; `scan` does it first thing). `None` if someone else claimed it, which
/// is logged: 0xCF8 writes are then refused.
pub fn cf8_writer() -> Option<&'static hal::portio::PciConfigWriter> {
    CF8_WRITER
        .get_or_init(|| {
            let w = hal::portio::PciConfigWriter::claim();
            if w.is_none() {
                klog::event!(Error, "pci", "cf8.no_writer"; "hal's 0xCF8 writer was claimed outside the pci crate: legacy configuration writes are refused");
            }
            w
        })
        .as_ref()
}

impl ConfigAccess for PortCf8 {
    fn read32(&self, at: Loc, off: u16) -> u32 {
        if at.segment != 0 || off >= STD_SPACE || at.dev > 31 || at.func > 7 {
            return u32::MAX;
        }
        hal::portio::pci_config_read32(at.bus, at.dev, at.func, off as u8)
    }
    fn write32(&self, at: Loc, off: u16, v: u32) -> bool {
        if at.segment != 0 || off >= STD_SPACE || at.dev > 31 || at.func > 7 {
            return false;
        }
        let Some(w) = cf8_writer() else { return false };
        hal::portio::pci_config_write32(w, at.bus, at.dev, at.func, off as u8, v);
        true
    }
    fn space_size(&self, segment: u16, _bus: u8) -> u16 {
        if segment == 0 { STD_SPACE } else { 0 }
    }
    fn read32_bounded(&self, at: Loc, off: u16, give_up: &dyn Fn() -> bool) -> Option<u32> {
        if at.segment != 0 || off >= STD_SPACE || at.dev > 31 || at.func > 7 {
            return Some(u32::MAX);
        }
        hal::portio::pci_config_read32_bounded(at.bus, at.dev, at.func, off as u8, give_up)
    }
    fn write32_bounded(&self, at: Loc, off: u16, v: u32, give_up: &dyn Fn() -> bool) -> Option<bool> {
        if at.segment != 0 || off >= STD_SPACE || at.dev > 31 || at.func > 7 {
            return Some(false);
        }
        // Never `cf8_writer()` here: its first call may log. `scan` claimed it long before any quiesce.
        let Some(w) = CF8_WRITER.get().and_then(Option::as_ref) else { return Some(false) };
        hal::portio::pci_config_write32_bounded(w, at.bus, at.dev, at.func, off as u8, v, give_up).map(|()| true)
    }
    fn name(&self) -> &'static str {
        "cf8"
    }
}
