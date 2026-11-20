//! The two rings on real memory, by the rules of `desc`. No IOMMU stands between this card and RAM, so every address
//! and length is checked when the memory is taken (`new`), and a buffer's address always comes from its index, never
//! from what a descriptor says. Linux re-reads the address from the descriptor (M:4830); writing it again on every
//! hand-back costs one store and makes the chip's write-back irrelevant.
//!
//! No klog here (`poll_tx` and Quiesce share these paths); outcomes are returned.

use crate::desc::{self, Rx, DESC_BYTES, NUM_RX, NUM_TX, RING_ALIGN, RING_BYTES, RX_BUF_SIZE};
use crate::hw::Hw;
use crate::mem::DmaMem;
use crate::seq;
use alloc::vec::Vec;
use core::sync::atomic::{compiler_fence, Ordering};

/// One receive buffer's share of its memory: the 16 383 bytes the descriptor names, rounded to 16 KiB.
pub const RX_STRIDE: usize = RX_BUF_SIZE as usize + 1;
/// Receive buffers come in runs of this many (1 MiB): four allocations instead of one of 4 MiB.
pub const RX_CHUNK_BUFS: usize = 64;
pub const RX_CHUNK_BYTES: usize = RX_CHUNK_BUFS * RX_STRIDE;
pub const RX_CHUNKS: usize = NUM_RX as usize / RX_CHUNK_BUFS;
/// One transmit slot: a whole frame is copied in (1518 bytes at most, `desc::tx_plan`).
pub const TX_SLOT: usize = 2048;
pub const TX_BUF_BYTES: usize = NUM_TX as usize * TX_SLOT;
/// The shortest frame the stack may hand over or get: an Ethernet header.
pub const MIN_FRAME: usize = 14;
/// The longest frame handed up: 1500 + header + one VLAN tag. Longer ones only arrive when a sender ignores the MTU.
pub const RX_MAX_FRAME: usize = seq::MTU as usize + 14 + 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// A descriptor or buffer access fell outside its memory (never expected; refused, not a panic).
    Memory,
    /// The memory handed to `new` cannot be a ring.
    Layout(&'static str),
}

/// A piece of DMA memory of at least `len` bytes that the card may be told about: not at address 0, not wrapping,
/// wholly below `limit`.
fn usable<M: DmaMem>(m: &M, len: usize, limit: u64) -> bool {
    let end = m.dev_addr().checked_add(len as u64);
    m.dev_addr() != 0 && m.size() >= len && end.is_some_and(|e| e <= limit)
}

fn ring_ok<M: DmaMem>(desc: &M, limit: u64) -> Result<(), RingError> {
    if !usable(desc, RING_BYTES, limit) {
        return Err(RingError::Layout("descriptor memory too small, at 0 or above the card's reach"));
    }
    if !desc.dev_addr().is_multiple_of(RING_ALIGN as u64) {
        return Err(RingError::Layout("descriptor memory not 256-byte aligned"));
    }
    Ok(())
}

fn zero_desc<M: DmaMem>(desc: &M, entry: u32) -> bool {
    let d = entry as usize * DESC_BYTES;
    desc.wr32(d, 0) && desc.wr32(d + 4, 0) && desc.wr64(d + 8, 0)
}

// ---------------------------------------------------------------------------------------------------------------
// Receive

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxOut {
    /// The next descriptor is still the chip's.
    Empty,
    /// A good frame of this length was copied into the caller's buffer.
    Frame(usize),
    /// A descriptor came back unusable and was handed to the chip again.
    Dropped(RxDrop),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxDrop {
    Error { crc: bool },
    Fragmented,
    BadLength,
    /// Good, but the caller's buffer is too small.
    TooBig,
}

pub struct RxRing<M: DmaMem> {
    desc: M,
    chunks: Vec<M>,
    next: u32,
}

impl<M: DmaMem> RxRing<M> {
    /// `desc`: 4 KiB for 256 descriptors; `chunks`: `RX_CHUNKS` runs of `RX_CHUNK_BYTES`.
    pub fn new(desc: M, chunks: Vec<M>, limit: u64) -> Result<Self, RingError> {
        ring_ok(&desc, limit)?;
        if chunks.len() != RX_CHUNKS || !chunks.iter().all(|c| usable(c, RX_CHUNK_BYTES, limit)) {
            return Err(RingError::Layout("receive buffers: wrong count, too small, at 0 or above the card's reach"));
        }
        Ok(RxRing { desc, chunks, next: 0 })
    }

    pub fn desc_addr(&self) -> u64 {
        self.desc.dev_addr()
    }

    fn place(entry: u32) -> (usize, usize) {
        (entry as usize / RX_CHUNK_BUFS, entry as usize % RX_CHUNK_BUFS * RX_STRIDE)
    }

    pub fn buf_addr(&self, entry: u32) -> u64 {
        let (chunk, off) = Self::place(entry);
        self.chunks[chunk].dev_addr() + off as u64
    }

    /// `rtl8169_mark_to_asic` (M:4151-4159) with the address written again: `opts2`, the address, then after a
    /// barrier the word that gives the descriptor away.
    fn arm(&self, entry: u32) -> bool {
        let d = entry as usize * DESC_BYTES;
        let fresh = desc::rx_init(entry, self.buf_addr(entry));
        let ok = self.desc.wr32(d + 4, 0) && self.desc.wr64(d + 8, fresh.addr);
        compiler_fence(Ordering::SeqCst);
        ok && self.desc.wr32(d, fresh.opts1)
    }

    /// Every descriptor to the chip, the ring read from its start: only while the chip is in reset (`rtl_reset_work`,
    /// M:4314-4315, and `rtl8169_init_ring_indexes`).
    pub fn arm_all(&mut self) -> Result<(), RingError> {
        self.next = 0;
        if (0..NUM_RX).all(|i| self.arm(i)) { Ok(()) } else { Err(RingError::Memory) }
    }

    /// `rtl_rx` for one descriptor (M:4771-4855).
    pub fn take(&mut self, buf: &mut [u8]) -> Result<RxOut, RingError> {
        let entry = self.next;
        let opts1 = self.desc.rd32(entry as usize * DESC_BYTES).ok_or(RingError::Memory)?;
        let out = match desc::rx_decode(opts1) {
            Rx::Owned => return Ok(RxOut::Empty),
            Rx::Error { crc, .. } => RxOut::Dropped(RxDrop::Error { crc }),
            Rx::Fragmented => RxOut::Dropped(RxDrop::Fragmented),
            Rx::TooShort => RxOut::Dropped(RxDrop::BadLength),
            Rx::Frame(n) if !(MIN_FRAME..=RX_MAX_FRAME).contains(&n) => RxOut::Dropped(RxDrop::BadLength),
            Rx::Frame(n) if n > buf.len() => RxOut::Dropped(RxDrop::TooBig),
            Rx::Frame(n) => {
                // The ownership bit was seen before the data is read (Linux `dma_rmb`, M:4792).
                compiler_fence(Ordering::SeqCst);
                let (chunk, off) = Self::place(entry);
                if !self.chunks[chunk].copy_out(off, &mut buf[..n]) {
                    return Err(RingError::Memory);
                }
                RxOut::Frame(n)
            }
        };
        if !self.arm(entry) {
            return Err(RingError::Memory);
        }
        self.next = (entry + 1) % NUM_RX;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Transmit

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxError {
    /// 256 frames with the card.
    Full,
    Memory,
    /// Empty or longer than the MTU allows (`desc::tx_plan`).
    BadFrame,
}

pub struct TxRing<M: DmaMem> {
    desc: M,
    bufs: M,
    cur: desc::TxCursor,
    /// When the card last finished a frame, or when the ring went from empty to busy.
    progress_ns: u64,
    /// Frames the card finished since the driver started (not reset with the ring).
    done: u64,
}

impl<M: DmaMem> TxRing<M> {
    pub fn new(desc: M, bufs: M, limit: u64) -> Result<Self, RingError> {
        ring_ok(&desc, limit)?;
        if !usable(&bufs, TX_BUF_BYTES, limit) {
            return Err(RingError::Layout("transmit buffers too small, at 0 or above the card's reach"));
        }
        Ok(TxRing { desc, bufs, cur: desc::TxCursor::default(), progress_ns: 0, done: 0 })
    }

    pub fn desc_addr(&self) -> u64 {
        self.desc.dev_addr()
    }

    pub fn in_flight(&self) -> u32 {
        self.cur.in_flight()
    }

    pub fn progress_ns(&self) -> u64 {
        self.progress_ns
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    /// The link is back: what the card still holds counts as handed over now.
    pub fn touch(&mut self, now_ns: u64) {
        self.progress_ns = now_ns;
    }

    /// `rtl8169_tx_clear` and the index reset (M:4263-4267, :4302-4303): only while the chip is in reset.
    pub fn reset(&mut self, now_ns: u64) -> Result<(), RingError> {
        self.cur = desc::TxCursor::default();
        self.progress_ns = now_ns;
        if (0..NUM_TX).all(|i| zero_desc(&self.desc, i)) { Ok(()) } else { Err(RingError::Memory) }
    }

    /// `rtl_tx` (M:4709-4754): takes back what the card has sent. The extra doorbell is Linux's "TxPoll requests are
    /// lost when the Tx packets are too close". Returns how many came back.
    pub fn reclaim<H: Hw + ?Sized>(&mut self, hw: &H, now_ns: u64) -> Result<usize, TxError> {
        let mut n = 0;
        while let Some(entry) = self.cur.oldest_entry() {
            let opts1 = self.desc.rd32(entry as usize * DESC_BYTES).ok_or(TxError::Memory)?;
            if !desc::tx_done(opts1) {
                break;
            }
            if !zero_desc(&self.desc, entry) {
                return Err(TxError::Memory);
            }
            self.cur.reclaimed();
            n += 1;
        }
        if n > 0 {
            self.done += n as u64;
            self.progress_ns = now_ns;
            if self.cur.in_flight() != 0 {
                seq::doorbell(hw);
            }
        }
        Ok(n)
    }

    /// `rtl8169_start_xmit` for one frame in one descriptor (M:4559-4629): the frame is copied into the slot of its
    /// descriptor and padded with zeros to 60 bytes (M:4448, the chip's own rule when no offload is on).
    pub fn put<H: Hw + ?Sized>(&mut self, hw: &H, frame: &[u8], now_ns: u64) -> Result<(), TxError> {
        if frame.len() < MIN_FRAME {
            return Err(TxError::BadFrame);
        }
        let entry = match self.cur.next_entry() {
            Some(e) => e,
            None => {
                self.reclaim(hw, now_ns)?;
                self.cur.next_entry().ok_or(TxError::Full)?
            }
        };
        let slot = entry as usize * TX_SLOT;
        let plan = desc::tx_plan(entry, self.bufs.dev_addr() + slot as u64, frame.len()).ok_or(TxError::BadFrame)?;
        const ZEROS: [u8; 64] = [0; 64];
        let pad = plan.len - frame.len();
        if !self.bufs.copy_in(slot, frame) || (pad > 0 && !self.bufs.copy_in(slot + frame.len(), &ZEROS[..pad])) {
            return Err(TxError::Memory);
        }
        let d = entry as usize * DESC_BYTES;
        let ok = self.desc.wr64(d + 8, plan.addr) && self.desc.wr32(d + 4, plan.opts2) && self.desc.wr32(d, plan.opts1_prepare);
        // M:4604: "Force memory writes to complete before releasing descriptor".
        compiler_fence(Ordering::SeqCst);
        if !(ok && self.desc.wr32(d, plan.opts1_release)) {
            return Err(TxError::Memory);
        }
        compiler_fence(Ordering::SeqCst);
        if self.cur.in_flight() == 0 {
            self.progress_ns = now_ns;
        }
        self.cur.sent();
        seq::doorbell(hw);
        Ok(())
    }
}
