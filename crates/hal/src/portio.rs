//! Port I/O (owner: PC). Safe functions with V1's register guard (`spec/core.md` §4): never the ATA/floppy
//! ports (and 0xCF8-0xCFF only through `pci_config_*`), so no safe caller can reach a disk controller.
//!
//! PCI configuration space through 0xCF8/0xCFC: the address/data pair is taken under a hal-internal lock
//! with interrupts off, so no caller can split another's pair. Reading is open to everyone; writing needs
//! the one `PciConfigWriter` there is, which the pci crate claims at its scan (R8: class 01 and the opted-out
//! functions are written only through the pci crate's checks, never through a raw tool).

use core::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortRefused(pub u16);

fn refused(port: u16) -> bool {
    matches!(port, 0x170..=0x177 | 0x1F0..=0x1F7 | 0x370..=0x377 | 0x3F0..=0x3F7 | 0xCF8..=0xCFF)
}

pub(crate) mod raw {
    #[cfg(target_os = "none")]
    pub fn outb(port: u16, v: u8) {
        // SAFETY: port writes have no memory effects; callers are the guarded safe API and hal-internal users
        // with fixed ports (serial, debug exit, PIC).
        unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") v, options(nomem, nostack, preserves_flags)) }
    }
    #[cfg(target_os = "none")]
    pub fn inb(port: u16) -> u8 {
        let v: u8;
        // SAFETY: as outb.
        unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") v, options(nomem, nostack, preserves_flags)) };
        v
    }
    #[cfg(target_os = "none")]
    pub fn outw(port: u16, v: u16) {
        // SAFETY: as outb.
        unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") v, options(nomem, nostack, preserves_flags)) }
    }
    #[cfg(target_os = "none")]
    pub fn inw(port: u16) -> u16 {
        let v: u16;
        // SAFETY: as outb.
        unsafe { core::arch::asm!("in ax, dx", in("dx") port, out("ax") v, options(nomem, nostack, preserves_flags)) };
        v
    }
    #[cfg(target_os = "none")]
    pub fn outl(port: u16, v: u32) {
        // SAFETY: as outb.
        unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") v, options(nomem, nostack, preserves_flags)) }
    }
    #[cfg(target_os = "none")]
    pub fn inl(port: u16) -> u32 {
        let v: u32;
        // SAFETY: as outb.
        unsafe { core::arch::asm!("in eax, dx", in("dx") port, out("eax") v, options(nomem, nostack, preserves_flags)) };
        v
    }
    #[cfg(not(target_os = "none"))]
    pub fn outb(_: u16, _: u8) {}
    #[cfg(not(target_os = "none"))]
    pub fn inb(_: u16) -> u8 {
        0xFF
    }
    #[cfg(not(target_os = "none"))]
    pub fn outw(_: u16, _: u16) {}
    #[cfg(not(target_os = "none"))]
    pub fn inw(_: u16) -> u16 {
        0xFFFF
    }
    #[cfg(not(target_os = "none"))]
    pub fn outl(_: u16, _: u32) {}
    #[cfg(not(target_os = "none"))]
    pub fn inl(_: u16) -> u32 {
        0xFFFF_FFFF
    }
}

macro_rules! guarded {
    ($in:ident, $out:ident, $t:ty) => {
        pub fn $in(port: u16) -> Result<$t, PortRefused> {
            if refused(port) { Err(PortRefused(port)) } else { Ok(raw::$in(port)) }
        }
        pub fn $out(port: u16, v: $t) -> Result<(), PortRefused> {
            if refused(port) { Err(PortRefused(port)) } else { raw::$out(port, v); Ok(()) }
        }
    };
}
guarded!(inb, outb, u8);
guarded!(inw, outw, u16);
guarded!(inl, outl, u32);

/// Serialises the 0xCF8 address / 0xCFC data pair across CPUs; interrupts are off while it is held.
static CF8_BUSY: AtomicBool = AtomicBool::new(false);
static WRITER_CLAIMED: AtomicBool = AtomicBool::new(false);

fn cf8_locked<R>(f: impl FnOnce() -> R) -> R {
    let _irq = crate::cpu::IrqGuard::new();
    while CF8_BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let r = f();
    CF8_BUSY.store(false, Ordering::Release);
    r
}

/// `cf8_locked` for the quiesce path: gives up (`None`) once `give_up()` says so instead of spinning on a
/// pair lock a stopped CPU may hold.
fn cf8_try_locked<R>(give_up: &dyn Fn() -> bool, f: impl FnOnce() -> R) -> Option<R> {
    let _irq = crate::cpu::IrqGuard::new();
    while CF8_BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        if give_up() {
            return None;
        }
        core::hint::spin_loop();
    }
    let r = f();
    CF8_BUSY.store(false, Ordering::Release);
    Some(r)
}

fn cf8_address(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000u32 | (bus as u32) << 16 | ((dev as u32) & 31) << 11 | ((func as u32) & 7) << 8 | (offset as u32 & 0xFC)
}

/// The right to write PCI configuration space through 0xCF8/0xCFC. Exactly one exists; the pci crate claims
/// it when it scans (a claim anywhere else is a review finding, and the pci crate then logs that it has none).
#[derive(Debug)]
pub struct PciConfigWriter {
    _only_hal_builds_it: (),
}

impl PciConfigWriter {
    /// The writer, once; `None` for every later claim.
    pub fn claim() -> Option<PciConfigWriter> {
        WRITER_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            .then_some(PciConfigWriter { _only_hal_builds_it: () })
    }
}

/// Legacy PCI configuration read through 0xCF8/0xCFC (the pair is never split, see the module comment).
pub fn pci_config_read32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let addr = cf8_address(bus, dev, func, offset);
    cf8_locked(|| {
        raw::outl(0xCF8, addr);
        raw::inl(0xCFC)
    })
}

/// Legacy PCI configuration write; only the holder of the `PciConfigWriter` can call it.
pub fn pci_config_write32(_writer: &PciConfigWriter, bus: u8, dev: u8, func: u8, offset: u8, value: u32) {
    let addr = cf8_address(bus, dev, func, offset);
    cf8_locked(|| {
        raw::outl(0xCF8, addr);
        raw::outl(0xCFC, value);
    })
}

/// `pci_config_read32` with a bounded wait for the pair (quiesce path); `None`: gave up, nothing accessed.
pub fn pci_config_read32_bounded(bus: u8, dev: u8, func: u8, offset: u8, give_up: &dyn Fn() -> bool) -> Option<u32> {
    let addr = cf8_address(bus, dev, func, offset);
    cf8_try_locked(give_up, || {
        raw::outl(0xCF8, addr);
        raw::inl(0xCFC)
    })
}

/// `pci_config_write32` with a bounded wait for the pair (quiesce path); `None`: gave up, nothing written.
pub fn pci_config_write32_bounded(_writer: &PciConfigWriter, bus: u8, dev: u8, func: u8, offset: u8, value: u32, give_up: &dyn Fn() -> bool) -> Option<()> {
    let addr = cf8_address(bus, dev, func, offset);
    cf8_try_locked(give_up, || {
        raw::outl(0xCF8, addr);
        raw::outl(0xCFC, value);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disk_ports_are_refused() {
        assert_eq!(outb(0x1F7, 0), Err(PortRefused(0x1F7)));
        assert_eq!(inb(0x3F5), Err(PortRefused(0x3F5)));
        assert!(outb(0x80, 0).is_ok());
    }

    #[test]
    fn one_config_writer_and_the_pair_lock_is_released() {
        let w = PciConfigWriter::claim();
        assert!(w.is_some());
        assert!(PciConfigWriter::claim().is_none());
        assert_eq!(pci_config_read32(0, 0, 0, 0), 0xFFFF_FFFF);
        pci_config_write32(w.as_ref().unwrap(), 0, 0, 0, 4, 0);
        assert!(!CF8_BUSY.load(Ordering::Relaxed));
        assert_eq!(cf8_address(0x12, 3, 4, 0x47), 0x8012_1C44);
        // In the same test: the pair lock is global, a parallel test would see it held.
        CF8_BUSY.store(true, Ordering::Relaxed);
        let tries = core::cell::Cell::new(0u32);
        let give_up = || {
            tries.set(tries.get() + 1);
            tries.get() > 3
        };
        assert_eq!(pci_config_read32_bounded(0, 0, 0, 0, &give_up), None);
        assert_eq!(tries.get(), 4);
        CF8_BUSY.store(false, Ordering::Relaxed);
        assert_eq!(pci_config_read32_bounded(0, 0, 0, 0, &|| true), Some(0xFFFF_FFFF), "a free pair is taken at once");
        assert!(!CF8_BUSY.load(Ordering::Relaxed));
    }
}
