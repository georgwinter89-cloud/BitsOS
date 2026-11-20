//! Ring and buffer memory as the ring code sees it: `mm::dma::DmaBuffer` in the kernel, `VecMem` in host tests (which
//! the fake card reads and writes as its DMA). Every access is checked; `None` or `false` means the offset was
//! outside the buffer, which the ring code treats as an error, never as a panic.

pub trait DmaMem {
    /// The address the card uses.
    fn dev_addr(&self) -> u64;
    fn size(&self) -> usize;
    fn rd32(&self, off: usize) -> Option<u32>;
    fn wr32(&self, off: usize, v: u32) -> bool;
    fn wr64(&self, off: usize, v: u64) -> bool;
    fn copy_in(&self, off: usize, src: &[u8]) -> bool;
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool;
}

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
    // Explicit paths: the trait and `DmaBuffer` share these names.
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        mm::dma::DmaBuffer::copy_in(self, off, src)
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        mm::dma::DmaBuffer::copy_out(self, off, dst)
    }
}

/// Host-test memory: bytes at a made-up device address, shared (clones see the same bytes) so the fake card can
/// reach it by address like a bus.
#[cfg(test)]
#[derive(Clone)]
pub struct VecMem {
    pub addr: u64,
    pub bytes: alloc::sync::Arc<ksync::SpinLock<alloc::vec::Vec<u8>>>,
}

#[cfg(test)]
impl VecMem {
    pub fn new(addr: u64, size: usize) -> Self {
        VecMem { addr, bytes: alloc::sync::Arc::new(ksync::SpinLock::new(alloc::vec![0xEE; size])) }
    }
    fn put(&self, off: usize, v: &[u8], align: bool) -> bool {
        let mut b = self.bytes.lock();
        if align && !off.is_multiple_of(v.len()) {
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
        self.bytes.lock().len()
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        let b = self.bytes.lock();
        off.is_multiple_of(4).then_some(())?;
        Some(u32::from_le_bytes(b.get(off..off.checked_add(4)?)?.try_into().ok()?))
    }
    fn wr32(&self, off: usize, v: u32) -> bool {
        self.put(off, &v.to_le_bytes(), true)
    }
    fn wr64(&self, off: usize, v: u64) -> bool {
        self.put(off, &v.to_le_bytes(), true)
    }
    fn copy_in(&self, off: usize, src: &[u8]) -> bool {
        self.put(off, src, false)
    }
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool {
        let b = self.bytes.lock();
        match off.checked_add(dst.len()).and_then(|e| b.get(off..e)) {
            Some(s) => {
                dst.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
}
