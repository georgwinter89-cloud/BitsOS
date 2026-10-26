//! DMA memory as the ring and context code sees it. In the kernel it is a `mm::dma::DmaBuffer` accessed through
//! MM-4's typed accessors (one naturally aligned volatile access per call, bounds-checked by the buffer's own
//! unforgeable `DmaRegion`); host tests use the fake controller's memory. Every access is checked: `None`/`false`
//! means the offset was outside the buffer, which the callers treat as an error, never as a panic.

use crate::trb::{Trb, CYCLE};

pub trait DmaMem: Send + Sync {
    /// The address the controller uses.
    fn dev_addr(&self) -> u64;
    fn size(&self) -> usize;
    fn rd32(&self, off: usize) -> Option<u32>;
    fn wr32(&self, off: usize, v: u32) -> bool;
    fn wr64(&self, off: usize, v: u64) -> bool;
    /// Byte copies (data stages, descriptors).
    fn read_bytes(&self, off: usize, out: &mut [u8]) -> bool;
    fn write_bytes(&self, off: usize, data: &[u8]) -> bool;

    fn zero(&self) -> bool {
        (0..self.size() / 4).all(|i| self.wr32(i * 4, 0))
    }

    /// Writes a TRB so the controller sees it whole: parameter and status first, the control dword (with the
    /// cycle bit that hands it over) last as one store. Volatile stores stay in program order and x86 does not
    /// reorder stores, so no fence is needed between them (the buffers are write-back).
    fn write_trb(&self, off: usize, t: &Trb) -> bool {
        self.wr32(off, t.param as u32)
            && self.wr32(off + 4, (t.param >> 32) as u32)
            && self.wr32(off + 8, t.status)
            && self.wr32(off + 12, t.control)
    }

    /// Reads an event TRB if its cycle bit is `cycle`: the control dword first (one load), the rest after it.
    fn read_trb_if(&self, off: usize, cycle: bool) -> Option<Option<Trb>> {
        let control = self.rd32(off + 12)?;
        if (control & CYCLE != 0) != cycle {
            return Some(None);
        }
        let lo = self.rd32(off)?;
        let hi = self.rd32(off + 4)?;
        let status = self.rd32(off + 8)?;
        Some(Some(Trb { param: (hi as u64) << 32 | lo as u64, status, control }))
    }
}

/// Every buffer is write-back (`driver.rs` asks for it), so plain volatile stores in program order reach the
/// controller in order; no `write_barrier` is needed before a doorbell. `device_addr()` is the physical address
/// while the IOMMU is off.
impl DmaMem for mm::dma::DmaBuffer {
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
    fn wr64(&self, off: usize, v: u64) -> bool {
        self.write_u64(off, v)
    }
    fn read_bytes(&self, off: usize, out: &mut [u8]) -> bool {
        mm::dma::DmaBuffer::copy_out(self, off, out)
    }
    fn write_bytes(&self, off: usize, data: &[u8]) -> bool {
        mm::dma::DmaBuffer::copy_in(self, off, data)
    }
    fn zero(&self) -> bool {
        mm::dma::DmaBuffer::zero(self)
    }
}
