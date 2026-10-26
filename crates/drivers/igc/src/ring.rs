//! Receive and transmit queue 0 (`V1:igcring.asm`, spec net.md §2.2 "Rings and data path"): 16-byte advanced
//! descriptors, one 2 KB buffer per descriptor, ring sizes as parameters (§3). Two hardware facts shape it:
//! - the descriptor write-back overwrites the buffer address, so a buffer is always derived from the ring
//!   index, never read back from a descriptor (`V1:igcring.asm:132-137, 197`);
//! - a tail reaching the head makes a full ring look empty to the card. V1 checked the next slot's DD; here the
//!   driver counts what it gave the card and never gives more than `count - 1` (the same rule, by count).
//!
//! No klog here (`poll_tx` and Quiesce share these paths); outcomes are returned.

use crate::hw::{until, Hw};
use crate::mem::DmaMem;
use crate::regs::{self, BUF_SIZE, DESC_SIZE};
use core::sync::atomic::{compiler_fence, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// The queue's enable bit did not come up within 100 ms.
    NotEnabled,
    /// A descriptor or buffer access fell outside its DMA memory (never expected; refused, not a panic).
    Memory,
}

/// Ring-size rules of the card: a multiple of 8 descriptors (RDLEN/TDLEN in 128-byte units), at least 8.
pub fn valid_count(n: usize) -> bool {
    n >= 8 && n.is_multiple_of(8) && n <= 32768
}

const ENABLE_WAIT_US: u64 = 100_000;

fn addr_regs<H: Hw + ?Sized>(hw: &H, lo: usize, hi: usize, a: u64) {
    hw.wr(lo, a as u32);
    hw.wr(hi, (a >> 32) as u32);
}

// ---------------------------------------------------------------------------------------------------------
// Receive

/// What one look at the receive ring found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rx {
    /// No descriptor written back.
    Empty,
    /// A good frame of this length was copied into the caller's buffer.
    Frame(usize),
    /// A written-back descriptor was dropped and given back (reason for the counters).
    Dropped(RxDrop),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxDrop {
    /// No end of packet: a frame spread over buffers (multi-buffer frames are dropped).
    NotEop,
    /// RXE: a frame error.
    Error,
    /// Shorter than 14 or longer than 1518.
    BadLength,
    /// Good, but the caller's buffer is too small.
    TooBig,
}

pub struct RxRing<M: DmaMem> {
    desc: M,
    bufs: M,
    count: usize,
    /// Next descriptor the card fills.
    next: usize,
    /// Descriptors given back since RDT was last written, and the last one given.
    pending: usize,
    last_given: usize,
}

impl<M: DmaMem> RxRing<M> {
    /// `desc` holds `count` descriptors, `bufs` `count` buffers of 2 KB.
    pub fn new(desc: M, bufs: M, count: usize) -> Result<Self, RingError> {
        if !valid_count(count) || desc.size() < count * DESC_SIZE || bufs.size() < count * BUF_SIZE {
            return Err(RingError::Memory);
        }
        Ok(RxRing { desc, bufs, count, next: 0, pending: 0, last_given: count - 1 })
    }
    pub fn count(&self) -> usize {
        self.count
    }
    pub fn next(&self) -> usize {
        self.next
    }
    fn buf_addr(&self, i: usize) -> u64 {
        self.bufs.dev_addr() + (i * BUF_SIZE) as u64
    }
    /// Read format again: the buffer from the index, header address 0 (which also clears DD).
    fn arm(&self, i: usize) -> bool {
        self.desc.wr64(i * DESC_SIZE, self.buf_addr(i)) && self.desc.wr64(i * DESC_SIZE + 8, 0)
    }

    /// Descriptors in read format and queue 0 programmed (`V1:igcring.asm:205-220`): RXDCTL off first (it may
    /// come enabled), base, length, head 0, advanced one-buffer descriptors, enable with WTHRESH 1 and wait for
    /// it, RCTL on, RDT = count - 1 (one descriptor held back so a full ring never looks empty).
    pub fn start<H: Hw + ?Sized>(&mut self, hw: &H) -> Result<(), RingError> {
        for i in 0..self.count {
            if !self.arm(i) {
                return Err(RingError::Memory);
            }
        }
        self.next = 0;
        self.pending = 0;
        self.last_given = self.count - 1;
        compiler_fence(Ordering::SeqCst);
        hw.wr(regs::RXDCTL, 0);
        addr_regs(hw, regs::RDBAL, regs::RDBAH, self.desc.dev_addr());
        hw.wr(regs::RDLEN, (self.count * DESC_SIZE) as u32);
        hw.wr(regs::RDH, 0);
        hw.wr(regs::RDT, 0);
        hw.wr(regs::SRRCTL, regs::SRRCTL_ADV_2K);
        hw.wr(regs::RXDCTL, regs::QUEUE_ENABLE | regs::RX_WTHRESH1);
        if !until(hw, ENABLE_WAIT_US, 100, || hw.rd(regs::RXDCTL) & regs::QUEUE_ENABLE != 0) {
            return Err(RingError::NotEnabled);
        }
        hw.wr(regs::RCTL, regs::RCTL_ON);
        hw.wr(regs::RDT, (self.count - 1) as u32);
        Ok(())
    }

    /// Looks at the next descriptor; a written-back one is consumed (copied or dropped) and given back.
    pub fn take(&mut self, buf: &mut [u8]) -> Result<Rx, RingError> {
        let i = self.next;
        let st = self.desc.rd32(i * DESC_SIZE + 8).ok_or(RingError::Memory)?;
        if st & regs::DESC_DD == 0 {
            return Ok(Rx::Empty);
        }
        // DD seen before the rest is read: loads are ordered on x86, the fence keeps the compiler from moving
        // them (Linux `dma_rmb`).
        compiler_fence(Ordering::SeqCst);
        let len = self.desc.rd16(i * DESC_SIZE + 12).ok_or(RingError::Memory)? as usize;
        let out = if st & regs::RX_EOP == 0 {
            Rx::Dropped(RxDrop::NotEop)
        } else if st & regs::RX_RXE != 0 {
            Rx::Dropped(RxDrop::Error)
        } else if !(14..=regs::RX_MAX_FRAME).contains(&len) {
            Rx::Dropped(RxDrop::BadLength)
        } else if len > buf.len() {
            Rx::Dropped(RxDrop::TooBig)
        } else if self.bufs.copy_out(i * BUF_SIZE, &mut buf[..len]) {
            Rx::Frame(len)
        } else {
            return Err(RingError::Memory);
        };
        if !self.arm(i) {
            return Err(RingError::Memory);
        }
        self.last_given = i;
        self.pending += 1;
        self.next = (i + 1) % self.count;
        Ok(out)
    }

    /// Descriptors given back but not yet announced (RDT writes are batched: an MMIO write is a VM exit).
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Hands the given-back descriptors to the card: RDT = the last one given (it stays held back).
    pub fn flush<H: Hw + ?Sized>(&mut self, hw: &H) {
        if self.pending > 0 {
            compiler_fence(Ordering::SeqCst);
            hw.wr(regs::RDT, self.last_given as u32);
            self.pending = 0;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Transmit

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxError {
    /// `count - 1` frames with the card: no free descriptor.
    Full,
    Memory,
}

pub struct TxRing<M: DmaMem> {
    desc: M,
    bufs: M,
    count: usize,
    /// Next descriptor to fill (the tail we write).
    tail: usize,
    /// Oldest descriptor the card still has.
    clean: usize,
    in_flight: usize,
    /// When the card last finished a frame, or when the ring went from empty to busy.
    progress_ns: u64,
}

impl<M: DmaMem> TxRing<M> {
    pub fn new(desc: M, bufs: M, count: usize) -> Result<Self, RingError> {
        if !valid_count(count) || desc.size() < count * DESC_SIZE || bufs.size() < count * BUF_SIZE {
            return Err(RingError::Memory);
        }
        Ok(TxRing { desc, bufs, count, tail: 0, clean: 0, in_flight: 0, progress_ns: 0 })
    }
    pub fn count(&self) -> usize {
        self.count
    }
    /// Frames the card holds.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
    /// How many frames fit at most: one descriptor stays empty (tail == head means empty to the card).
    pub fn capacity(&self) -> usize {
        self.count - 1
    }
    pub fn progress_ns(&self) -> u64 {
        self.progress_ns
    }
    fn buf_addr(&self, i: usize) -> u64 {
        self.bufs.dev_addr() + (i * BUF_SIZE) as u64
    }

    /// Queue 0 programmed (`V1:igcring.asm:221-234`): TXDCTL off, base, length, head = tail = 0, enable with
    /// WTHRESH 0 (each descriptor with RS is written back), wait, then TCTL on with short-packet padding.
    pub fn start<H: Hw + ?Sized>(&mut self, hw: &H, now_ns: u64) -> Result<(), RingError> {
        for i in 0..self.count {
            if !(self.desc.wr64(i * DESC_SIZE, self.buf_addr(i)) && self.desc.wr64(i * DESC_SIZE + 8, 0)) {
                return Err(RingError::Memory);
            }
        }
        self.tail = 0;
        self.clean = 0;
        self.in_flight = 0;
        self.progress_ns = now_ns;
        compiler_fence(Ordering::SeqCst);
        hw.wr(regs::TXDCTL, 0);
        addr_regs(hw, regs::TDBAL, regs::TDBAH, self.desc.dev_addr());
        hw.wr(regs::TDLEN, (self.count * DESC_SIZE) as u32);
        hw.wr(regs::TDH, 0);
        hw.wr(regs::TDT, 0);
        hw.wr(regs::TXDCTL, regs::QUEUE_ENABLE);
        if !until(hw, ENABLE_WAIT_US, 100, || hw.rd(regs::TXDCTL) & regs::QUEUE_ENABLE != 0) {
            return Err(RingError::NotEnabled);
        }
        hw.set_bits(regs::TCTL, regs::TCTL_EN | regs::TCTL_PSP);
        Ok(())
    }

    /// Takes back what the card has sent (DD of the oldest frames). Returns how many.
    pub fn reclaim(&mut self, now_ns: u64) -> Result<usize, TxError> {
        let mut n = 0;
        while self.in_flight > 0 {
            let st = self.desc.rd32(self.clean * DESC_SIZE + 12).ok_or(TxError::Memory)?;
            if st & regs::DESC_DD == 0 {
                break;
            }
            self.clean = (self.clean + 1) % self.count;
            self.in_flight -= 1;
            n += 1;
        }
        if n > 0 {
            self.progress_ns = now_ns;
        }
        Ok(n)
    }

    /// Copies one frame (14..=1518 bytes, checked by the caller) into the next buffer and gives it to the card.
    /// Below 17 bytes it is padded with zeros to 17: the MAC refuses shorter ones even with PSP.
    pub fn put<H: Hw + ?Sized>(&mut self, hw: &H, frame: &[u8], now_ns: u64) -> Result<(), TxError> {
        if self.in_flight >= self.capacity() {
            self.reclaim(now_ns)?;
            if self.in_flight >= self.capacity() {
                return Err(TxError::Full);
            }
        }
        let i = self.tail;
        let mut len = frame.len();
        if !self.bufs.copy_in(i * BUF_SIZE, frame) {
            return Err(TxError::Memory);
        }
        if len < regs::TX_MIN_LEN {
            let zeros = [0u8; regs::TX_MIN_LEN];
            if !self.bufs.copy_in(i * BUF_SIZE + len, &zeros[..regs::TX_MIN_LEN - len]) {
                return Err(TxError::Memory);
            }
            len = regs::TX_MIN_LEN;
        }
        let d = i * DESC_SIZE;
        // The address from the index: the last write-back may have overwritten it.
        let ok = self.desc.wr64(d, self.buf_addr(i))
            && self.desc.wr32(d + 8, regs::TX_CMD_DATA | len as u32)
            && self.desc.wr32(d + 12, (len as u32) << regs::TX_PAYLEN_SHIFT);
        if !ok {
            return Err(TxError::Memory);
        }
        if self.in_flight == 0 {
            self.progress_ns = now_ns;
        }
        self.in_flight += 1;
        self.tail = (i + 1) % self.count;
        // Descriptor before the tail (Linux `wmb` before writing the tail; stores are ordered on x86).
        compiler_fence(Ordering::SeqCst);
        hw.wr(regs::TDT, self.tail as u32);
        Ok(())
    }
}
