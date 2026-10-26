//! The kernel's `Hw`: BAR0 through `hal::mmio::IoMem` (uncached), the clock from `time`, sleeping waits from
//! `sched`, DMA memory from `mm::dma` (MM-4: write-back when snooped, uncached rings and lists and
//! write-combining sample rings without snooping), PC-2's enable/disable.

use crate::hw::{AllocError, DmaUse, Hw, Mem};
use alloc::format;
use alloc::string::String;
use hal::mmio::IoMem;
use mm::dma::{DmaBuffer, DmaCache, DmaConstraints, DmaError};

/// `pause_us` sleeps from this length on and spins below it (a trip through the scheduler costs more).
const SLEEP_MIN_US: u64 = 50;

pub struct MmioHw {
    io: IoMem,
    pci: pci::PciFunction,
}

impl MmioHw {
    /// Maps BAR0, which must hold every stream descriptor GCAP names (checked after the map: GCAP is in it).
    pub fn map(pci: &pci::PciFunction) -> Result<MmioHw, String> {
        let Some(pci::Bar::Mem { phys, size, .. }) = pci.bar(0) else {
            return Err("BAR0 is not a memory BAR".into());
        };
        if size < 0x80 {
            return Err(format!("BAR0 is {size:#x} bytes"));
        }
        let io = hal::mmio::map_device(phys, size as usize, hal::paging::CacheType::Uncached)
            .map_err(|e| format!("map BAR0 {phys:#x}+{size:#x}: {e:?}"))?;
        // Memory decode before the first read: the Mini-PC's firmware leaves it off on both HDA functions
        // (command 0x0000), so every read gave all ones; QEMU turns it on itself. Bus master stays off here.
        pci.enable_device(false).map_err(|e| format!("enable_device(false): {e}"))?;
        let gcap = crate::regs::Gcap::parse(io.read16(crate::regs::GCAP).unwrap_or(0));
        let need = crate::regs::sd(gcap.total()) as u64;
        if size < need {
            return Err(format!("BAR0 is {size:#x} bytes, GCAP's {} descriptors need {need:#x}", gcap.total()));
        }
        Ok(MmioHw { io, pci: pci.clone() })
    }
    pub fn pci(&self) -> &pci::PciFunction {
        &self.pci
    }
}

impl Mem for DmaBuffer {
    fn dev_addr(&self) -> u64 {
        self.device_addr()
    }
    fn size(&self) -> usize {
        self.len()
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        self.read_u32(off)
    }
    fn wr32(&self, off: usize, v: u32) -> bool {
        self.write_u32(off, v)
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        DmaBuffer::copy_in(self, off, src)
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        DmaBuffer::copy_out(self, off, dst)
    }
    fn fill(&self, off: usize, n: usize, byte: u8) -> bool {
        DmaBuffer::fill(self, off, n, byte)
    }
    fn barrier(&self) {
        self.write_barrier()
    }
}

impl Hw for MmioHw {
    type Mem = DmaBuffer;
    fn rd8(&self, off: usize) -> u8 {
        self.io.read8(off).unwrap_or(!0)
    }
    fn rd16(&self, off: usize) -> u16 {
        self.io.read16(off).unwrap_or(!0)
    }
    fn rd32(&self, off: usize) -> u32 {
        self.io.read32(off).unwrap_or(!0)
    }
    fn wr8(&self, off: usize, v: u8) {
        let _ = self.io.write8(off, v);
    }
    fn wr16(&self, off: usize, v: u16) {
        let _ = self.io.write16(off, v);
    }
    fn wr32(&self, off: usize, v: u32) {
        let _ = self.io.write32(off, v);
    }
    fn now_ns(&self) -> u64 {
        time::now_ns()
    }
    fn delay_us(&self, us: u64) {
        time::delay_us(us)
    }
    fn pause_us(&self, us: u64) {
        if us >= SLEEP_MIN_US { sched::sleep_ns(us * 1000) } else { time::delay_us(us) }
    }
    fn alloc(&self, len: usize, usage: DmaUse, limit32: bool, snoop: bool) -> Result<DmaBuffer, (AllocError, String)> {
        let cache = match (snoop, usage) {
            (true, _) => DmaCache::WriteBack,
            (false, DmaUse::Control) => DmaCache::Uncached,
            (false, DmaUse::Data) => DmaCache::WriteCombining,
        };
        let base = if limit32 { DmaConstraints::dma32() } else { DmaConstraints::dma64() };
        DmaBuffer::alloc(len, base.with_align(crate::regs::BDL_ALIGN as u64).with_cache(cache)).map_err(|e| {
            let kind = if e == DmaError::Unsupported { AllocError::Unsupported } else { AllocError::Other };
            (kind, format!("DMA {len} bytes {cache:?}: {e:?}"))
        })
    }
    fn enable(&self, dma: bool) -> Result<(), String> {
        self.pci.enable_device(dma).map(|_| ()).map_err(|e| format!("enable_device({dma}): {e}"))
    }
    fn pci_off(&self) {
        let _ = self.pci.disable_device();
    }
    fn pci_modify32(&self, off: u8, clear: u32, set: u32) {
        // Vendor bytes (TCSEL, ATI snoop) the PCI core never writes, so a plain read-modify-write is enough.
        let v = self.pci.read32(off);
        let n = (v & !clear) | set;
        if n != v {
            self.pci.write32(off, n);
        }
    }
}
