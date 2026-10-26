//! The chip's DMA memory: pieces of contiguous buffers below 4 GB (spec/wlan.md §2.1 "DMA
//! addressing": the 8852BE behind the Mini-PC's AMD bridge reaches no more; Linux gives it 36-bit
//! DMA only behind Intel/ASMedia bridges, WZ:linux pci.c:3296-3336). rtw89-core asks for many small
//! regions (ring descriptors, 2 KB TX buffers, 4 KB RX buffers); each is carved from a chunk of
//! [`CHUNK`] bytes, so a ring of 256 buffers is not 256 page allocations. A chunk goes back when
//! its last piece does (the stop frees every ring). Write-back memory as Linux's
//! dma_alloc_coherent on x86 (PCIe snoops it); zeroed on hand-out as dma_alloc_coherent's.
//!
//! The arithmetic is [`Carve`] (host-tested); the buffers are mm's `DmaBuffer`.

use alloc::vec::Vec;
use mm::dma::{DmaBuffer, DmaConstraints};
use rtw89_core::bus::{BusError, BusResult};

/// One chunk's size (16 pages); larger requests get a chunk of their own
pub const CHUNK: usize = 64 * 1024;
const PAGE: usize = 4096;

/// Where pieces go inside one chunk: a bump pointer and a count of live pieces
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Carve {
    pub base: u32,
    pub len: usize,
    pub next: usize,
    pub live: usize,
}

impl Carve {
    /// The offset for `size` bytes at `align` (a power of two), if it fits
    pub fn take(&mut self, size: usize, align: usize) -> Option<usize> {
        let a = align.max(1);
        let base = self.base as usize;
        let start = (base + self.next).checked_add(a - 1)? & !(a - 1);
        let off = start - base;
        let end = off.checked_add(size)?;
        if end > self.len {
            return None;
        }
        self.next = end;
        self.live += 1;
        Some(off)
    }
    pub fn holds(&self, addr: u32, len: usize) -> Option<usize> {
        let off = addr.checked_sub(self.base)? as usize;
        (off.checked_add(len)? <= self.len).then_some(off)
    }
}

struct Chunk {
    buf: DmaBuffer,
    carve: Carve,
}

#[derive(Default)]
pub struct DmaPool {
    chunks: Vec<Chunk>,
    /// the chunk the last access hit (RX polls read one buffer many times)
    last: usize,
}

fn exhausted(size: usize, align: usize) -> BusError {
    BusError::DmaExhausted { size, align }
}

impl DmaPool {
    pub fn alloc(&mut self, size: usize, align: usize) -> BusResult<u32> {
        if size == 0 || align == 0 || !align.is_power_of_two() {
            return Err(exhausted(size, align));
        }
        let own = size > CHUNK / 2 || align > PAGE;
        if !own {
            for i in (0..self.chunks.len()).rev() {
                let c = &mut self.chunks[i];
                if let Some(off) = c.carve.take(size, align) {
                    c.buf.fill(off, size, 0);
                    return Ok(c.carve.base + off as u32);
                }
            }
        }
        let len = if own { size.div_ceil(PAGE) * PAGE } else { CHUNK };
        let c = DmaConstraints::dma32().with_align(align.max(PAGE) as u64);
        let buf = DmaBuffer::alloc(len, c).map_err(|_| exhausted(size, align))?;
        let dev = buf.device_addr();
        if dev.checked_add(len as u64).is_none_or(|e| e > 1 << 32) {
            return Err(exhausted(size, align));
        }
        let mut carve = Carve {
            base: dev as u32,
            len,
            next: 0,
            live: 0,
        };
        let off = carve.take(size, align).ok_or_else(|| exhausted(size, align))?;
        buf.fill(off, size, 0);
        self.chunks.push(Chunk { buf, carve });
        Ok(carve.base + off as u32)
    }

    pub fn free(&mut self, addr: u32, size: usize) -> BusResult<()> {
        let i = self
            .chunks
            .iter()
            .position(|c| c.carve.holds(addr, size).is_some())
            .ok_or(BusError::Refused { kind: "dma free", addr, len: size as u32 })?;
        let c = &mut self.chunks[i];
        c.carve.live = c.carve.live.saturating_sub(1);
        if c.carve.live == 0 {
            // the chip no longer reaches it: rtw89-core frees only after bus master is off
            self.chunks.swap_remove(i);
            self.last = 0;
        }
        Ok(())
    }

    fn find(&mut self, addr: u32, len: usize) -> BusResult<(&DmaBuffer, usize)> {
        if let Some(c) = self.chunks.get(self.last)
            && let Some(off) = c.carve.holds(addr, len)
        {
            return Ok((&self.chunks[self.last].buf, off));
        }
        let i = self
            .chunks
            .iter()
            .position(|c| c.carve.holds(addr, len).is_some())
            .ok_or(BusError::Refused { kind: "dma", addr, len: len as u32 })?;
        self.last = i;
        let c = &self.chunks[i];
        let off = c.carve.holds(addr, len).unwrap_or(0);
        Ok((&c.buf, off))
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) -> BusResult<()> {
        let (b, off) = self.find(addr, data.len())?;
        if b.write(off, data) { Ok(()) } else { Err(BusError::Refused { kind: "dma", addr, len: data.len() as u32 }) }
    }

    pub fn read(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()> {
        let n = out.len();
        let (b, off) = self.find(addr, n)?;
        if b.read(off, out) { Ok(()) } else { Err(BusError::Refused { kind: "dma", addr, len: n as u32 }) }
    }

    /// Zeros over every live chunk (Quiesce: no key copy may stay in freed memory)
    pub fn wipe_all(&mut self) {
        for c in &self.chunks {
            let _ = c.buf.zero();
        }
    }

    pub fn chunks(&self) -> usize {
        self.chunks.len()
    }
    pub fn bytes(&self) -> usize {
        self.chunks.iter().map(|c| c.carve.len).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_are_aligned_and_inside() {
        let mut c = Carve { base: 0x1000_0000, len: CHUNK, next: 0, live: 0 };
        assert_eq!(c.take(8 * 16, 128), Some(0));
        assert_eq!(c.take(2176, 128), Some(128));
        assert_eq!(c.take(10, 8), Some(128 + 2176));
        let o = c.take(4096, 4096).unwrap();
        assert_eq!(o % 4096, 0);
        assert_eq!(c.live, 4);
        assert_eq!(c.take(CHUNK, 8), None, "past the chunk");
        assert_eq!(c.holds(0x1000_0000 + 100, 28), Some(100));
        assert_eq!(c.holds(0x1000_0000 + CHUNK as u32 - 4, 8), None);
        assert_eq!(c.holds(0x0fff_ffff, 1), None);
    }

    #[test]
    fn a_base_that_is_not_aligned_still_gives_aligned_addresses() {
        let mut c = Carve { base: 0x2000_0040, len: 4096, next: 0, live: 0 };
        let o = c.take(16, 128).unwrap();
        assert_eq!((0x2000_0040 + o) % 128, 0);
    }
}
