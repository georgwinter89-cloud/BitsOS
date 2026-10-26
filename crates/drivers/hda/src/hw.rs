//! What the controller logic needs from the machine: the registers in their widths, a clock, DMA memory and
//! the PCI steps. The kernel implements it over `hal::mmio::IoMem`, `mm::dma` and `pci` (`MmioHw`); host tests
//! with a register-level fake controller (`fake.rs`), so reset, CORB/RIRB, BDLs and positions run the same code.

use alloc::string::String;

/// Which caching DMA memory gets. `Snooped` is write-back; without snooping the controller reads RAM past the
/// CPU caches, so the rings and lists are uncached and the sample data write-combining (Linux `set_memory_wc`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaUse {
    /// CORB, RIRB, BDLs.
    Control,
    /// Sample rings.
    Data,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AllocError {
    /// Uncached/write-combining memory cannot be had now (MM-4 `Unsupported`): a caller may fall back to snooping.
    Unsupported,
    Other,
}

/// DMA memory as the driver sees it; every access is checked (false/None: outside the buffer).
pub trait Mem: Send + Sync {
    fn dev_addr(&self) -> u64;
    fn size(&self) -> usize;
    fn rd32(&self, off: usize) -> Option<u32>;
    fn wr32(&self, off: usize, v: u32) -> bool;
    fn copy_in(&self, off: usize, src: &[u8]) -> bool;
    /// Reads what the device wrote (capture rings, SN-9).
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool;
    fn fill(&self, off: usize, n: usize, byte: u8) -> bool;
    /// Makes earlier writes visible to the device (drains write-combining buffers) before a doorbell.
    fn barrier(&self) {}
}

pub trait Hw: Send + Sync {
    type Mem: Mem;
    fn rd8(&self, off: usize) -> u8;
    fn rd16(&self, off: usize) -> u16;
    /// All ones if the window is unusable (as a controller gone from the bus reads).
    fn rd32(&self, off: usize) -> u32;
    fn wr8(&self, off: usize, v: u8);
    fn wr16(&self, off: usize, v: u16);
    fn wr32(&self, off: usize, v: u32);
    fn now_ns(&self) -> u64;
    /// Spins (Quiesce, hard-IRQ-free short waits).
    fn delay_us(&self, us: u64);
    /// A wait in which the caller may sleep (thread context only). Default: spin.
    fn pause_us(&self, us: u64) {
        self.delay_us(us)
    }
    /// `len` zeroed bytes; `limit32` keeps them below 4 GiB (no 64OK in GCAP); `snoop` false asks for UC/WC.
    fn alloc(&self, len: usize, usage: DmaUse, limit32: bool, snoop: bool) -> Result<Self::Mem, (AllocError, String)>;
    /// PC-2 `enable_device(dma)`. Thread context, may sleep.
    fn enable(&self, dma: bool) -> Result<(), String>;
    /// PC-2 `disable_device`: MSI off, bus master off. Lock-free, no klog, no sleep (Quiesce).
    fn pci_off(&self);
    /// Config space dword read-modify-write (TCSEL, ATI snoop byte). Thread context.
    fn pci_modify32(&self, _off: u8, _clear: u32, _set: u32) {}
}

/// Polls `cond` every `step_us` until it holds or `limit_us` passed by the clock (never a count alone).
pub fn until<H: Hw + ?Sized>(hw: &H, limit_us: u64, step_us: u64, sleep: bool, mut cond: impl FnMut() -> bool) -> bool {
    let end = hw.now_ns().saturating_add(limit_us.saturating_mul(1000));
    loop {
        if cond() {
            return true;
        }
        if hw.now_ns() >= end {
            return cond();
        }
        if sleep { hw.pause_us(step_us) } else { hw.delay_us(step_us) }
    }
}
