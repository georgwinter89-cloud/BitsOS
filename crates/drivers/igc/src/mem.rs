//! Ring and buffer memory as the ring code sees it. In the kernel a `mm::dma::DmaBuffer` reached through its
//! typed accessors (MM-4: width-correct volatile accesses, `rep movsb` for payloads, which with ERMS/FSRM on both of
//! Georg's machines is the fastest copy without vector registers, R10); host tests use `VecMem`, which the
//! fake card reads and writes as its DMA. Every access is checked: `None`/`false` means the offset was outside
//! the buffer, which the ring code treats as an error, never as a panic.

pub trait DmaMem {
    /// The address the device uses.
    fn dev_addr(&self) -> u64;
    fn size(&self) -> usize;
    fn rd16(&self, off: usize) -> Option<u16>;
    fn rd32(&self, off: usize) -> Option<u32>;
    fn wr32(&self, off: usize, v: u32) -> bool;
    fn wr64(&self, off: usize, v: u64) -> bool;
    fn copy_in(&self, off: usize, src: &[u8]) -> bool;
    fn copy_out(&self, off: usize, dst: &mut [u8]) -> bool;
}

/// The kernel side: `DmaBuffer`'s typed accessors (MM-4) on its own CPU view. The rings are write-back
/// (`MmioHw::alloc_dma`), so no `write_barrier` is needed before a tail write.
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

/// Host-test memory: bytes at a made-up device address, shared (clones see the same bytes) so the fake card
/// can reach it by address like a bus.
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
    fn get<const N: usize>(&self, off: usize) -> Option<[u8; N]> {
        let b = self.bytes.lock();
        off.is_multiple_of(N).then_some(())?;
        b.get(off..off.checked_add(N)?)?.try_into().ok()
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
    fn rd16(&self, off: usize) -> Option<u16> {
        self.get::<2>(off).map(u16::from_le_bytes)
    }
    fn rd32(&self, off: usize) -> Option<u32> {
        self.get::<4>(off).map(u32::from_le_bytes)
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
