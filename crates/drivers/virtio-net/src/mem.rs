//! Ring and buffer memory as the queue code sees it. In the kernel this is a `mm::dma::DmaBuffer` accessed
//! through its typed accessors (width-correct volatile accesses, `rep movsb` for payloads); host tests use a plain
//! byte vector and play the device themselves. Every access is checked: `None`/`false` means the offset was
//! outside the buffer, which the queue code treats as a device error, never as a panic.

pub trait DmaMem {
    /// The address the device uses.
    fn dev_addr(&self) -> u64;
    fn size(&self) -> usize;
    fn rd16(&self, off: usize) -> Option<u16>;
    fn rd32(&self, off: usize) -> Option<u32>;
    fn wr16(&self, off: usize, v: u16) -> bool;
    fn wr32(&self, off: usize, v: u32) -> bool;
    fn wr64(&self, off: usize, v: u64) -> bool;
    fn copy_in(&self, off: usize, src: &[u8]) -> bool;
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool;
    /// Zeroes the whole buffer (queue reset).
    fn zero(&self) -> bool {
        let z = [0u8; 256];
        let mut off = 0;
        while off < self.size() {
            let n = z.len().min(self.size() - off);
            if !self.copy_in(off, &z[..n]) {
                return false;
            }
            off += n;
        }
        true
    }
}

/// The kernel side: `DmaBuffer`'s typed accessors (MM-4) on its own CPU view, one naturally aligned volatile
/// access per ring field (a 16-bit `avail.idx` is never written as two bytes) and `rep movsb` for payloads.
/// The device address stays `device_addr()` (IOMMU-ready); the CPU never derives its view from it.
impl DmaMem for mm::dma::DmaBuffer {
    fn dev_addr(&self) -> u64 {
        self.device_addr()
    }
    fn size(&self) -> usize {
        self.len()
    }
    fn rd16(&self, off: usize) -> Option<u16> {
        self.read_u16(off)
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        self.read_u32(off)
    }
    fn wr16(&self, off: usize, v: u16) -> bool {
        self.write_u16(off, v)
    }
    fn wr32(&self, off: usize, v: u32) -> bool {
        self.write_u32(off, v)
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        self.write_u64(off, v)
    }
    // Explicit paths: the trait and `DmaBuffer` share these names.
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        mm::dma::DmaBuffer::copy_in(self, off, src)
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        mm::dma::DmaBuffer::copy_out(self, off, dst)
    }
    fn zero(&self) -> bool {
        mm::dma::DmaBuffer::zero(self)
    }
}

/// Host-test memory: a byte vector at a made-up device address.
#[cfg(test)]
pub struct VecMem {
    pub addr: u64,
    pub bytes: core::cell::RefCell<alloc::vec::Vec<u8>>,
}

#[cfg(test)]
impl VecMem {
    pub fn new(addr: u64, size: usize) -> Self {
        VecMem { addr, bytes: core::cell::RefCell::new(alloc::vec![0xEE; size]) }
    }
    fn get<const N: usize>(&self, off: usize) -> Option<[u8; N]> {
        let b = self.bytes.borrow();
        off.is_multiple_of(N).then_some(())?;
        b.get(off..off.checked_add(N)?)?.try_into().ok()
    }
    fn put(&self, off: usize, v: &[u8]) -> bool {
        let mut b = self.bytes.borrow_mut();
        if !off.is_multiple_of(v.len()) {
            return false;
        }
        match off.checked_add(v.len()).and_then(|e| b.get_mut(off..e)) {
            Some(s) => {
                s.copy_from_slice(v);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
impl DmaMem for VecMem {
    fn dev_addr(&self) -> u64 {
        self.addr
    }
    fn size(&self) -> usize {
        self.bytes.borrow().len()
    }
    fn rd16(&self, off: usize) -> Option<u16> {
        self.get::<2>(off).map(u16::from_le_bytes)
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        self.get::<4>(off).map(u32::from_le_bytes)
    }
    fn wr16(&self, off: usize, v: u16) -> bool {
        self.put(off, &v.to_le_bytes())
    }
    fn wr32(&self, off: usize, v: u32) -> bool {
        self.put(off, &v.to_le_bytes())
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        self.put(off, &v.to_le_bytes())
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        let mut b = self.bytes.borrow_mut();
        match off.checked_add(src.len()).and_then(|e| b.get_mut(off..e)) {
            Some(s) => {
                s.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        let b = self.bytes.borrow();
        match off.checked_add(dst.len()).and_then(|e| b.get(off..e)) {
            Some(s) => {
                dst.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
}
