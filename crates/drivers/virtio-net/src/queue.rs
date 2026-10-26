//! Split virtqueue (virtio 1.2 §2.7), one descriptor per buffer: buffer `i` of the queue's buffer area always
//! belongs to descriptor `i`, so a frame never needs a chain (the 12-byte net header and the frame share one
//! buffer, as virtio 1.x allows). Descriptor ids the device hands back are checked against the ids in flight;
//! a device that lies gets an error, never a panic or an access outside the queue's memory.
//!
//! Ordering (x86, write-back coherent DMA): stores are not reordered with stores and loads not with loads (TSO),
//! so `wmb`/`rmb` only have to stop the compiler; the one full barrier is between publishing `avail.idx` and
//! reading the device's "no notify" flag (store -> load), as in Linux `virtqueue_kick_prepare`.

use crate::mem::DmaMem;
use alloc::vec::Vec;
use core::sync::atomic::{compiler_fence, fence, Ordering};

pub const DESC_F_WRITE: u16 = 2;
pub const USED_F_NO_NOTIFY: u16 = 1;
/// `avail.flags`: the driver does not want an interrupt when buffers are used (§2.7.7; a hint the device may
/// ignore, which only costs an extra interrupt).
pub const AVAIL_F_NO_INTERRUPT: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    /// A ring access fell outside the ring memory (sizes did not fit; a bug, reported instead of a panic).
    Mem,
    /// The device returned a descriptor id that is out of range or not in flight.
    BadId(u32),
    /// The device moved `used.idx` by more than the queue size.
    BadUsedIdx { last: u16, now: u16 },
    /// The queue size is not a power of two in 1..=32768, or the memory is too small for it.
    BadSize,
}

/// Bytes of the descriptor table, the available ring and the used ring for `n` entries.
pub const fn ring_bytes(n: usize) -> (usize, usize, usize) {
    (16 * n, 6 + 2 * n, 6 + 8 * n)
}

/// Who has a buffer: the free list, the driver (taken, or handed back by the device), or the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Free,
    Driver,
    Device,
}

pub struct VirtQueue<M> {
    pub index: u16,
    size: u16,
    desc: M,
    avail: M,
    used: M,
    bufs: M,
    buf_size: usize,
    free: Vec<u16>,
    slots: Vec<Slot>,
    avail_idx: u16,
    last_used: u16,
}

impl<M: DmaMem> VirtQueue<M> {
    /// A queue over the given memory; call `reset` before handing the addresses to the device.
    pub fn new(index: u16, size: u16, desc: M, avail: M, used: M, bufs: M, buf_size: usize) -> Result<Self, QueueError> {
        let n = size as usize;
        let (d, a, u) = ring_bytes(n);
        if !size.is_power_of_two() || size > 32768 || buf_size == 0 {
            return Err(QueueError::BadSize);
        }
        if desc.size() < d || avail.size() < a || used.size() < u || bufs.size() < n.saturating_mul(buf_size) {
            return Err(QueueError::BadSize);
        }
        let mut q = VirtQueue { index, size, desc, avail, used, bufs, buf_size, free: Vec::new(), slots: Vec::new(),
            avail_idx: 0, last_used: 0 };
        q.free.reserve(n);
        q.slots.resize(n, Slot::Free);
        Ok(q)
    }

    /// Zeroes the rings and points descriptor `i` at buffer `i`; every buffer is free afterwards.
    pub fn reset(&mut self) -> Result<(), QueueError> {
        if !(self.desc.zero() && self.avail.zero() && self.used.zero()) {
            return Err(QueueError::Mem);
        }
        self.free.clear();
        for id in (0..self.size).rev() {
            let addr = self.bufs.dev_addr() + id as u64 * self.buf_size as u64;
            if !self.desc.wr64(id as usize * 16, addr) {
                return Err(QueueError::Mem);
            }
            self.free.push(id);
        }
        self.slots.iter_mut().for_each(|s| *s = Slot::Free);
        self.avail_idx = 0;
        self.last_used = 0;
        Ok(())
    }

    pub fn size(&self) -> u16 {
        self.size
    }
    pub fn buf_size(&self) -> usize {
        self.buf_size
    }
    /// Device addresses of descriptor table, available ring ("driver area"), used ring ("device area").
    pub fn addrs(&self) -> (u64, u64, u64) {
        (self.desc.dev_addr(), self.avail.dev_addr(), self.used.dev_addr())
    }
    pub fn free_count(&self) -> usize {
        self.free.len()
    }
    pub fn in_flight_count(&self) -> usize {
        self.slots.iter().filter(|s| **s == Slot::Device).count()
    }

    pub fn take_free(&mut self) -> Option<u16> {
        let id = self.free.pop()?;
        self.slots[id as usize] = Slot::Driver;
        Some(id)
    }
    /// Gives a buffer the driver holds back to the free list (TX after the device used it, or never submitted).
    pub fn recycle(&mut self, id: u16) {
        if self.slots.get(id as usize) == Some(&Slot::Driver) {
            self.slots[id as usize] = Slot::Free;
            self.free.push(id);
        }
    }

    fn buf_off(&self, id: u16, off: usize, len: usize) -> Option<usize> {
        if id >= self.size || off.checked_add(len)? > self.buf_size {
            return None;
        }
        Some(id as usize * self.buf_size + off)
    }
    pub fn write_buf(&self, id: u16, off: usize, data: &[u8]) -> bool {
        self.buf_off(id, off, data.len()).is_some_and(|o| self.bufs.copy_in(o, data))
    }
    pub fn read_buf(&self, id: u16, off: usize, out: &mut [u8]) -> bool {
        self.buf_off(id, off, out.len()).is_some_and(|o| self.bufs.copy_out(o, out))
    }

    /// Offers buffer `id` (taken with `take_free`, or handed back by `pop_used`) to the device: `len` bytes, device-writable for RX, device-readable for TX.
    pub fn submit(&mut self, id: u16, len: u32, device_writes: bool) -> Result<(), QueueError> {
        let i = id as usize;
        if self.slots.get(i) != Some(&Slot::Driver) || len as usize > self.buf_size {
            return Err(QueueError::BadId(id as u32));
        }
        let flags = if device_writes { DESC_F_WRITE } else { 0 };
        let slot = 4 + 2 * (self.avail_idx % self.size) as usize;
        if !(self.desc.wr32(i * 16 + 8, len) && self.desc.wr16(i * 16 + 12, flags) && self.desc.wr16(i * 16 + 14, 0)
            && self.avail.wr16(slot, id))
        {
            return Err(QueueError::Mem);
        }
        // wmb: descriptor and ring entry before the index that publishes them.
        compiler_fence(Ordering::Release);
        self.avail_idx = self.avail_idx.wrapping_add(1);
        if !self.avail.wr16(2, self.avail_idx) {
            return Err(QueueError::Mem);
        }
        self.slots[i] = Slot::Device;
        Ok(())
    }

    /// After `submit`: does the device want a notification? (It may set `NO_NOTIFY` while it is busy anyway.)
    pub fn needs_notify(&self) -> bool {
        // mb: our avail.idx store must be visible before we read the device's flag (store -> load).
        fence(Ordering::SeqCst);
        self.used.rd16(0).is_none_or(|f| f & USED_F_NO_NOTIFY == 0)
    }

    /// Interrupts for used buffers on (`true`) or off (NW-4: off while the driver drains the ring, NAPI; Linux
    /// `virtqueue_disable_cb`/`virtqueue_enable_cb`). Turning them on returns whether used buffers are already
    /// waiting (the device may have used one before it saw the flag): then the caller looks again.
    pub fn set_interrupts(&mut self, on: bool) -> bool {
        let _ = self.avail.wr16(0, if on { 0 } else { AVAIL_F_NO_INTERRUPT });
        if !on {
            return false;
        }
        // mb: the flag store before the used.idx load (store -> load), as Linux `virtqueue_enable_cb`.
        fence(Ordering::SeqCst);
        self.has_used()
    }

    /// Used buffers wait to be taken.
    pub fn has_used(&self) -> bool {
        self.used.rd16(2).is_some_and(|i| i != self.last_used)
    }

    /// Takes the next buffer the device has finished with: `(id, bytes written by the device)`.
    pub fn pop_used(&mut self) -> Result<Option<(u16, u32)>, QueueError> {
        let now = self.used.rd16(2).ok_or(QueueError::Mem)?;
        if now == self.last_used {
            return Ok(None);
        }
        if now.wrapping_sub(self.last_used) > self.size {
            return Err(QueueError::BadUsedIdx { last: self.last_used, now });
        }
        // rmb: the element after the index that announced it.
        compiler_fence(Ordering::Acquire);
        let e = 4 + 8 * (self.last_used % self.size) as usize;
        let id = self.used.rd32(e).ok_or(QueueError::Mem)?;
        let len = self.used.rd32(e + 4).ok_or(QueueError::Mem)?;
        self.last_used = self.last_used.wrapping_add(1);
        if self.slots.get(id as usize) != Some(&Slot::Device) {
            return Err(QueueError::BadId(id));
        }
        self.slots[id as usize] = Slot::Driver;
        Ok(Some((id as u16, len)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mem::VecMem;

    pub fn queue(n: u16, buf: usize) -> VirtQueue<VecMem> {
        let (d, a, u) = ring_bytes(n as usize);
        let mut q = VirtQueue::new(1, n, VecMem::new(0x10_000, d), VecMem::new(0x20_000, a), VecMem::new(0x30_000, u),
            VecMem::new(0x100_000, n as usize * buf), buf)
        .unwrap();
        q.reset().unwrap();
        q
    }

    /// The device side: takes every newly available descriptor and returns it as used with `len`.
    pub struct FakeDev {
        pub seen_avail: u16,
        pub used_idx: u16,
    }
    impl FakeDev {
        pub fn new() -> Self {
            FakeDev { seen_avail: 0, used_idx: 0 }
        }
        /// Returns the ids consumed, with their descriptor (addr, len, flags).
        pub fn consume(&mut self, q: &VirtQueue<VecMem>, len: impl Fn(u16) -> u32) -> Vec<(u16, u64, u32, u16)> {
            let n = q.size;
            let avail_idx = q.avail.rd16(2).unwrap();
            let mut out = Vec::new();
            while self.seen_avail != avail_idx {
                let id = q.avail.rd16(4 + 2 * (self.seen_avail % n) as usize).unwrap();
                let d = id as usize * 16;
                let b = q.desc.bytes.borrow();
                let addr = u64::from_le_bytes(b[d..d + 8].try_into().unwrap());
                let dl = u32::from_le_bytes(b[d + 8..d + 12].try_into().unwrap());
                let fl = u16::from_le_bytes(b[d + 12..d + 14].try_into().unwrap());
                drop(b);
                out.push((id, addr, dl, fl));
                let e = 4 + 8 * (self.used_idx % n) as usize;
                q.used.wr32(e, id as u32);
                q.used.wr32(e + 4, len(id));
                self.used_idx = self.used_idx.wrapping_add(1);
                self.seen_avail = self.seen_avail.wrapping_add(1);
            }
            q.used.wr16(2, self.used_idx);
            out
        }
    }

    #[test]
    fn sizes_are_checked() {
        let (d, a, u) = ring_bytes(8);
        let mk = |n: u16, dsz: usize| {
            VirtQueue::new(0, n, VecMem::new(0, dsz), VecMem::new(0, a), VecMem::new(0, u), VecMem::new(0, 8 * 64), 64)
        };
        assert!(mk(8, d).is_ok());
        assert_eq!(mk(6, d).err(), Some(QueueError::BadSize), "not a power of two");
        assert_eq!(mk(8, d - 1).err(), Some(QueueError::BadSize), "descriptor table too small");
        assert_eq!(mk(16, d).err(), Some(QueueError::BadSize));
    }

    #[test]
    fn reset_points_descriptors_at_their_buffers() {
        let q = queue(4, 2048);
        assert_eq!(q.free_count(), 4);
        for id in 0..4usize {
            let b = q.desc.bytes.borrow();
            assert_eq!(u64::from_le_bytes(b[id * 16..id * 16 + 8].try_into().unwrap()), 0x100_000 + id as u64 * 2048);
        }
        assert_eq!(q.avail.rd16(2), Some(0));
    }

    #[test]
    fn submit_and_complete_round_trip_with_wraparound() {
        let mut q = queue(4, 64);
        let mut dev = FakeDev::new();
        // Many rounds so both 16-bit indices wrap past the ring size many times (and past 65535).
        for round in 0..70_000u32 {
            let id = q.take_free().unwrap();
            assert!(q.write_buf(id, 0, &round.to_le_bytes()));
            q.submit(id, 4, false).unwrap();
            let got = dev.consume(&q, |_| 0);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].0, id);
            assert_eq!(got[0].2, 4);
            assert_eq!(got[0].3, 0, "TX descriptors are device-readable");
            let (uid, _) = q.pop_used().unwrap().unwrap();
            assert_eq!(uid, id);
            q.recycle(uid);
            assert_eq!(q.pop_used().unwrap(), None);
        }
        assert_eq!(q.free_count(), 4);
        assert_eq!(q.in_flight_count(), 0);
    }

    #[test]
    fn rx_buffers_are_device_writable() {
        let mut q = queue(4, 2048);
        while let Some(id) = q.take_free() {
            q.submit(id, 2048, true).unwrap();
        }
        let mut dev = FakeDev::new();
        let got = dev.consume(&q, |id| 100 + id as u32);
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|g| g.3 == DESC_F_WRITE && g.2 == 2048));
        let mut lens = Vec::new();
        while let Some((id, len)) = q.pop_used().unwrap() {
            lens.push((id, len));
        }
        assert_eq!(lens.len(), 4);
        assert!(lens.iter().all(|(id, len)| *len == 100 + *id as u32));
    }

    #[test]
    fn a_lying_device_gets_errors_not_panics() {
        let mut q = queue(4, 64);
        let id = q.take_free().unwrap();
        q.submit(id, 4, false).unwrap();
        // id out of range
        q.used.wr32(4, 99);
        q.used.wr16(2, 1);
        assert_eq!(q.pop_used(), Err(QueueError::BadId(99)));
        // id not in flight
        let mut q = queue(4, 64);
        q.used.wr32(4, 2);
        q.used.wr16(2, 1);
        assert_eq!(q.pop_used(), Err(QueueError::BadId(2)));
        // index jumps by more than the ring size
        let mut q = queue(4, 64);
        q.used.wr16(2, 5);
        assert_eq!(q.pop_used(), Err(QueueError::BadUsedIdx { last: 0, now: 5 }));
    }

    #[test]
    fn double_submit_and_oversize_are_refused() {
        let mut q = queue(4, 64);
        let id = q.take_free().unwrap();
        q.submit(id, 4, false).unwrap();
        assert!(q.submit(id, 4, false).is_err(), "already in flight");
        let id2 = q.take_free().unwrap();
        assert!(q.submit(id2, 65, false).is_err(), "longer than the buffer");
        assert!(!q.write_buf(id2, 60, &[0; 8]), "write past the buffer");
        assert!(!q.write_buf(9, 0, &[0; 1]), "no such buffer");
    }

    /// NW-4: interrupts off writes the avail flag; on clears it and says whether used buffers already wait.
    #[test]
    fn interrupt_flag_and_used_check() {
        let mut q = queue(4, 64);
        assert!(!q.set_interrupts(false));
        assert_eq!(q.avail.rd16(0), Some(AVAIL_F_NO_INTERRUPT));
        assert!(!q.set_interrupts(true), "nothing used yet");
        assert_eq!(q.avail.rd16(0), Some(0));
        let id = q.take_free().unwrap();
        q.submit(id, 10, true).unwrap();
        let mut dev = FakeDev::new();
        dev.consume(&q, |_| 10);
        assert!(q.has_used());
        assert!(q.set_interrupts(true), "a buffer used before the flag went: look again");
        assert_eq!(q.pop_used().unwrap(), Some((id, 10)));
        assert!(!q.has_used());
    }

    #[test]
    fn no_notify_flag_is_honoured() {
        let q = queue(4, 64);
        assert!(q.needs_notify());
        q.used.wr16(0, USED_F_NO_NOTIFY);
        assert!(!q.needs_notify());
    }
}
