//! The modern virtio-pci transport over mapped BAR windows: device status, feature words, queue set-up and
//! notification (virtio 1.2 §4.1.4.3). Register offsets of `struct virtio_pci_common_cfg`.

use crate::caps::{Region, VirtioCaps};
use alloc::format;
use alloc::string::String;
use hal::mmio::IoMem;
use hal::paging::CacheType;

const DEVICE_FEATURE_SELECT: usize = 0x00;
const DEVICE_FEATURE: usize = 0x04;
const DRIVER_FEATURE_SELECT: usize = 0x08;
const DRIVER_FEATURE: usize = 0x0C;
const NUM_QUEUES: usize = 0x12;
const DEVICE_STATUS: usize = 0x14;
const CONFIG_GENERATION: usize = 0x15;
const QUEUE_SELECT: usize = 0x16;
const QUEUE_SIZE: usize = 0x18;
const QUEUE_MSIX_VECTOR: usize = 0x1A;
const QUEUE_ENABLE: usize = 0x1C;
const QUEUE_NOTIFY_OFF: usize = 0x1E;
const QUEUE_DESC: usize = 0x20;
const QUEUE_DRIVER: usize = 0x28;
const QUEUE_DEVICE: usize = 0x30;
const CONFIG_MSIX_VECTOR: usize = 0x10;

pub const NO_VECTOR: u16 = 0xFFFF;

/// Device status bits.
pub mod status {
    pub const ACKNOWLEDGE: u8 = 1;
    pub const DRIVER: u8 = 2;
    pub const DRIVER_OK: u8 = 4;
    pub const FEATURES_OK: u8 = 8;
    pub const NEEDS_RESET: u8 = 64;
    pub const FAILED: u8 = 128;
}

pub struct Transport {
    common: IoMem,
    notify: IoMem,
    notify_mult: u32,
    device: Option<IoMem>,
}

fn map(f: &pci::PciFunction, r: Region, what: &str) -> Result<IoMem, String> {
    let Some(pci::Bar::Mem { phys, size, .. }) = f.bar(r.bar) else {
        return Err(format!("{what}: BAR{} is not a memory BAR", r.bar));
    };
    if (r.offset as u64).checked_add(r.length as u64).is_none_or(|e| e > size) {
        return Err(format!("{what}: {:#x}+{:#x} outside BAR{} ({size:#x} bytes)", r.offset, r.length, r.bar));
    }
    hal::mmio::map_device(phys + r.offset as u64, r.length as usize, CacheType::Uncached)
        .map_err(|e| format!("{what}: map BAR{} failed: {e:?}", r.bar))
}

/// Map failures and register faults both end in this: the device is not usable.
fn io<T>(r: Result<T, hal::mmio::MmioError>) -> Result<T, devmodel::DevError> {
    r.map_err(|_| devmodel::DevError::Io)
}

impl Transport {
    pub fn map(f: &pci::PciFunction, c: &VirtioCaps) -> Result<Transport, String> {
        let common = map(f, c.common, "common cfg")?;
        let notify = map(f, c.notify, "notify")?;
        let device = match c.device {
            Some(r) => Some(map(f, r, "device cfg")?),
            None => None,
        };
        Ok(Transport { common, notify, notify_mult: c.notify_mult, device })
    }

    pub fn status(&self) -> u8 {
        // A device that fell off the bus reads all ones; callers treat FAILED|NEEDS_RESET as "restart".
        self.common.read8(DEVICE_STATUS).unwrap_or(0xFF)
    }
    pub fn set_status(&self, s: u8) -> Result<(), devmodel::DevError> {
        io(self.common.write8(DEVICE_STATUS, s))
    }
    pub fn add_status(&self, bits: u8) -> Result<(), devmodel::DevError> {
        self.set_status(self.status() | bits)
    }

    /// Writes 0 to the status and waits (spinning, never sleeping: Quiesce runs with interrupts off) until the
    /// device reads back 0 or `limit_ns` passes. QEMU completes the reset within the write.
    pub fn reset(&self, limit_ns: u64) -> bool {
        if self.set_status(0).is_err() {
            return false;
        }
        let start = time::now_ns();
        // The spin bound also ends the wait if the clock is not running yet (time::now_ns() == 0).
        for _ in 0..50_000_000u64 {
            if self.common.read8(DEVICE_STATUS) == Ok(0) {
                return true;
            }
            if time::now_ns().saturating_sub(start) > limit_ns {
                break;
            }
            hal::cpu::spin_hint();
        }
        false
    }

    pub fn device_features(&self) -> Result<u64, devmodel::DevError> {
        io(self.common.write32(DEVICE_FEATURE_SELECT, 0))?;
        let lo = io(self.common.read32(DEVICE_FEATURE))?;
        io(self.common.write32(DEVICE_FEATURE_SELECT, 1))?;
        let hi = io(self.common.read32(DEVICE_FEATURE))?;
        Ok((hi as u64) << 32 | lo as u64)
    }
    pub fn set_driver_features(&self, f: u64) -> Result<(), devmodel::DevError> {
        io(self.common.write32(DRIVER_FEATURE_SELECT, 0))?;
        io(self.common.write32(DRIVER_FEATURE, f as u32))?;
        io(self.common.write32(DRIVER_FEATURE_SELECT, 1))?;
        io(self.common.write32(DRIVER_FEATURE, (f >> 32) as u32))
    }
    pub fn num_queues(&self) -> u16 {
        self.common.read16(NUM_QUEUES).unwrap_or(0)
    }
    pub fn no_config_vector(&self) -> Result<(), devmodel::DevError> {
        io(self.common.write16(CONFIG_MSIX_VECTOR, NO_VECTOR))
    }
    /// Configuration changes (the link bit) to MSI-X table entry `v` (NW-4). The device answers NO_VECTOR when
    /// it cannot (§4.1.5.1.2): `Ok(false)`.
    pub fn config_vector(&self, v: u16) -> Result<bool, devmodel::DevError> {
        io(self.common.write16(CONFIG_MSIX_VECTOR, v))?;
        Ok(io(self.common.read16(CONFIG_MSIX_VECTOR))? == v)
    }

    /// Largest size the device allows for queue `q` (0 = the queue does not exist).
    pub fn queue_max(&self, q: u16) -> Result<u16, devmodel::DevError> {
        io(self.common.write16(QUEUE_SELECT, q))?;
        io(self.common.read16(QUEUE_SIZE))
    }

    /// Programs queue `q` (size, ring addresses, its MSI-X table entry `vector` or NO_VECTOR for polled) and
    /// enables it. Returns its notify offset in bytes inside the notify window. A vector the device does not take
    /// (it reads back NO_VECTOR, §4.1.5.1.3) is `Err(Unsupported)`.
    pub fn setup_queue(&self, q: u16, size: u16, desc: u64, driver: u64, device: u64, vector: u16) -> Result<usize, devmodel::DevError> {
        let c = &self.common;
        io(c.write16(QUEUE_SELECT, q))?;
        io(c.write16(QUEUE_SIZE, size))?;
        io(c.write16(QUEUE_MSIX_VECTOR, vector))?;
        if vector != NO_VECTOR && io(c.read16(QUEUE_MSIX_VECTOR))? != vector {
            return Err(devmodel::DevError::Unsupported);
        }
        for (reg, a) in [(QUEUE_DESC, desc), (QUEUE_DRIVER, driver), (QUEUE_DEVICE, device)] {
            // 64-bit fields as two 32-bit writes, low first (allowed by §4.1.3.1).
            io(c.write32(reg, a as u32))?;
            io(c.write32(reg + 4, (a >> 32) as u32))?;
        }
        let off = io(c.read16(QUEUE_NOTIFY_OFF))? as usize * self.notify_mult as usize;
        if off.checked_add(2).is_none_or(|e| e > self.notify.size()) {
            return Err(devmodel::DevError::Io);
        }
        io(c.write16(QUEUE_ENABLE, 1))?;
        Ok(off)
    }

    /// Tells the device queue `q` has new buffers. Lock-free (one MMIO write), so `poll_tx` may use it.
    pub fn notify(&self, notify_off: usize, q: u16) -> bool {
        self.notify.write16(notify_off, q).is_ok()
    }

    /// Reads `out.len()` bytes of the device configuration at `off`, retried until the configuration
    /// generation is stable (§4.1.4.3.1: the device may change it while we read).
    pub fn read_config(&self, off: usize, out: &mut [u8]) -> Result<(), devmodel::DevError> {
        let d = self.device.as_ref().ok_or(devmodel::DevError::Unsupported)?;
        for _ in 0..16 {
            let g0 = io(self.common.read8(CONFIG_GENERATION))?;
            for (i, b) in out.iter_mut().enumerate() {
                *b = io(d.read8(off + i))?;
            }
            if io(self.common.read8(CONFIG_GENERATION))? == g0 {
                return Ok(());
            }
        }
        Err(devmodel::DevError::Timeout)
    }
    pub fn config_u16(&self, off: usize) -> Result<u16, devmodel::DevError> {
        let d = self.device.as_ref().ok_or(devmodel::DevError::Unsupported)?;
        io(d.read16(off))
    }
    pub fn config_u32(&self, off: usize) -> Result<u32, devmodel::DevError> {
        let d = self.device.as_ref().ok_or(devmodel::DevError::Unsupported)?;
        io(d.read32(off))
    }
}
