//! Ring buffers as the driver writes them: geometry, 64-bit write pointer, reservation, NOP padding on commit,
//! IB padding, and the frame helpers (IB and fence packets with their alignment rules).
//!
//! Rules (linux-6.6/drivers/gpu/drm/amd/amdgpu/): a submission reserves `ndw` rounded up to the ring's
//! alignment (amdgpu_ring.c:81-90), writes at most that many dwords, and commit pads with NOPs to the
//! alignment before the write pointer moves (amdgpu_ring.c:140-147). Alignment: GFX 256 dwords with the
//! one-dword NOP 0xffff1000 (gfx_v11_0.c:6084-6085), SDMA 16 dwords with NOP 0 (sdma_v6_0.c:1557-1558).
//! IBs: SDMA pads to 8 (sdma_v6_0.c:1129), GFX to the alignment the caller names (amdgpu_ring.c:125).
//! Pointers are 64-bit dword counters; SDMA registers and writeback hold bytes (`<< 2`, sdma_v6_0.c:126/175),
//! GFX ones dwords. The ring size is the caller's choice (a power of two), nothing here fixes it (R1).

use alloc::vec::Vec;

use crate::encode::EncodeError;
use crate::{pm4, sdma};

/// SDMA ring alignment in dwords.
pub const SDMA_RING_ALIGN_DW: u64 = 16;
/// SDMA IB alignment in dwords.
pub const SDMA_IB_ALIGN_DW: u64 = 8;
/// GFX ring alignment in dwords (also Linux's IB padding for kernel IBs).
pub const GFX_RING_ALIGN_DW: u64 = 256;

/// Which engine's rules apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Gfx,
    /// `burst_nop`: the firmware takes the counted NOP (Linux reads it from the firmware header).
    Sdma { burst_nop: bool },
}

impl Kind {
    pub fn ring_align_dw(self) -> u64 {
        match self {
            Kind::Gfx => GFX_RING_ALIGN_DW,
            Kind::Sdma { .. } => SDMA_RING_ALIGN_DW,
        }
    }
    /// The hardware form of a pointer (bytes for SDMA, dwords for GFX), as written to the wptr register,
    /// doorbell or wptr poll slot.
    pub fn hw_ptr(self, ptr_dw: u64) -> u64 {
        match self {
            Kind::Gfx => ptr_dw,
            Kind::Sdma { .. } => ptr_dw << 2,
        }
    }
    /// A pointer read back from the hardware (rptr register or writeback) as a dword counter.
    pub fn ptr_from_hw(self, hw: u64) -> u64 {
        match self {
            Kind::Gfx => hw,
            Kind::Sdma { .. } => hw >> 2,
        }
    }
    /// Appends `n` NOP dwords in this engine's form.
    pub fn nops(self, out: &mut Vec<u32>, n: u64) {
        match self {
            Kind::Gfx => out.extend(core::iter::repeat_n(pm4::PAD, n as usize)),
            Kind::Sdma { burst_nop } => {
                // A burst NOP covers at most 0x4000 dwords; longer padding is split.
                let mut left = n;
                while left > 0 {
                    let chunk = left.min(sdma::NOP_MAX_COUNT as u64 + 1) as u32;
                    // Cannot fail: chunk - 1 <= NOP_MAX_COUNT.
                    let _ = sdma::nops(out, chunk, burst_nop);
                    left -= chunk as u64;
                }
            }
        }
    }
}

/// Pads an IB in place to a multiple of `align_dw` (a power of two; SDMA needs a multiple of 8).
pub fn pad_ib(kind: Kind, ib: &mut Vec<u32>, align_dw: u64) -> Result<(), EncodeError> {
    let ok = align_dw.is_power_of_two() && (!matches!(kind, Kind::Sdma { .. }) || align_dw.is_multiple_of(SDMA_IB_ALIGN_DW));
    if !ok {
        return Err(EncodeError::Field { what: "IB alignment", value: align_dw });
    }
    let pad = (align_dw - (ib.len() as u64 & (align_dw - 1))) & (align_dw - 1);
    kind.nops(ib, pad);
    Ok(())
}

/// Why a ring operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingError {
    /// Size not a power of two or below two alignment units.
    BadSize(u64),
    /// The reservation can never fit (Linux: -ENOMEM, amdgpu_ring.c:87).
    TooBig(u64),
    /// Not enough free space before the read pointer now; retry after the engine moved on.
    Full { need: u64, free: u64 },
    /// More dwords written than reserved (Linux prints "writing more dwords to the ring than expected").
    Overrun,
    /// No reservation open, or one already open.
    State,
    /// A packet encoder refused.
    Encode(EncodeError),
    /// The read pointer is not in [wptr - size, wptr].
    BadRptr(u64),
    /// The sink handed to commit holds fewer dwords than the ring; nothing was written, the reservation
    /// stays open (retry with the right sink or undo).
    SinkTooSmall { have: u64, need: u64 },
}

impl RingError {
    pub fn errno(&self) -> i32 {
        use crate::errno::*;
        match self {
            RingError::TooBig(_) => ENOMEM,
            RingError::Full { .. } => EBUSY,
            RingError::BadRptr(_) => EIO,
            RingError::Encode(e) => e.errno(),
            _ => EINVAL,
        }
    }
}

impl From<EncodeError> for RingError {
    fn from(e: EncodeError) -> Self {
        RingError::Encode(e)
    }
}

/// Where a commit lands: the ring memory the driver writes through. [`Ring::commit`] checks
/// `len_dw() >= size_dw` first, so `put` only ever sees `index < len_dw()`.
pub trait RingSink {
    /// Dwords the sink can hold.
    fn len_dw(&self) -> u64;
    fn put(&mut self, index: u64, dw: u32);
}

impl RingSink for [u32] {
    fn len_dw(&self) -> u64 {
        self.len() as u64
    }
    fn put(&mut self, index: u64, dw: u32) {
        if let Some(slot) = self.get_mut(index as usize) {
            *slot = dw;
        }
    }
}
impl RingSink for Vec<u32> {
    fn len_dw(&self) -> u64 {
        self.len() as u64
    }
    fn put(&mut self, index: u64, dw: u32) {
        self.as_mut_slice().put(index, dw)
    }
}

/// The driver side of one ring.
#[derive(Clone, Debug)]
pub struct Ring {
    kind: Kind,
    size_dw: u64,
    wptr: u64,
    staged: Vec<u32>,
    reserved: Option<u64>,
}

impl Ring {
    /// A ring of `size_dw` dwords (power of two, at least the alignment) whose pointers start at `start`
    /// (the value the hardware's rptr and wptr were set to; a multiple of the alignment).
    pub fn new(kind: Kind, size_dw: u64, start: u64) -> Result<Ring, RingError> {
        if !size_dw.is_power_of_two() || size_dw < 2 * kind.ring_align_dw() {
            return Err(RingError::BadSize(size_dw));
        }
        if start & (kind.ring_align_dw() - 1) != 0 {
            return Err(RingError::BadRptr(start));
        }
        Ok(Ring { kind, size_dw, wptr: start, staged: Vec::new(), reserved: None })
    }
    pub fn kind(&self) -> Kind {
        self.kind
    }
    pub fn size_dw(&self) -> u64 {
        self.size_dw
    }
    pub fn mask(&self) -> u64 {
        self.size_dw - 1
    }
    /// Committed write pointer (dword counter).
    pub fn wptr(&self) -> u64 {
        self.wptr
    }
    /// Position the next written dword gets.
    pub fn pos(&self) -> u64 {
        self.wptr.wrapping_add(self.staged.len() as u64)
    }
    /// Free dwords given the engine's read pointer. The ring is never filled completely (one alignment unit
    /// stays free so that full and empty never look alike to a masked pointer).
    pub fn free_dw(&self, rptr: u64) -> Result<u64, RingError> {
        let used = self.wptr.wrapping_sub(rptr);
        if used > self.size_dw {
            return Err(RingError::BadRptr(rptr));
        }
        Ok(self.size_dw - self.kind.ring_align_dw() - used.min(self.size_dw - self.kind.ring_align_dw()))
    }

    /// Opens a submission of up to `ndw` dwords (rounded up to the alignment).
    pub fn begin(&mut self, ndw: u64, rptr: u64) -> Result<(), RingError> {
        if self.reserved.is_some() {
            return Err(RingError::State);
        }
        let a = self.kind.ring_align_dw();
        let n = ndw.checked_add(a - 1).ok_or(RingError::TooBig(ndw))? & !(a - 1);
        if n > self.size_dw - a {
            return Err(RingError::TooBig(ndw));
        }
        let free = self.free_dw(rptr)?;
        if n > free {
            return Err(RingError::Full { need: n, free });
        }
        self.reserved = Some(n);
        Ok(())
    }

    /// Appends dwords to the open submission.
    pub fn write(&mut self, dws: &[u32]) -> Result<(), RingError> {
        let n = self.reserved.ok_or(RingError::State)?;
        if self.staged.len() as u64 + dws.len() as u64 > n {
            return Err(RingError::Overrun);
        }
        self.staged.extend_from_slice(dws);
        Ok(())
    }

    /// Runs an encoder into the open submission; on any error nothing is appended.
    pub fn emit(&mut self, f: impl FnOnce(&mut Vec<u32>) -> Result<(), EncodeError>) -> Result<(), RingError> {
        let mut tmp = Vec::new();
        f(&mut tmp)?;
        self.write(&tmp)
    }

    /// Pads with NOPs to the alignment, copies the submission into `sink` (wrapping) and moves the write
    /// pointer; returns the new write pointer (dword counter; [`Kind::hw_ptr`] gives the register value).
    pub fn commit<S: RingSink + ?Sized>(&mut self, sink: &mut S) -> Result<u64, RingError> {
        let n = self.reserved.ok_or(RingError::State)?;
        // A short sink would drop dwords silently while wptr still moved: the engine would run stale words.
        if sink.len_dw() < self.size_dw {
            return Err(RingError::SinkTooSmall { have: sink.len_dw(), need: self.size_dw });
        }
        let a = self.kind.ring_align_dw();
        let pad = (a - (self.pos() & (a - 1))) & (a - 1);
        // The pad never exceeds the reservation: wptr is aligned and the reservation is a multiple of a.
        if self.staged.len() as u64 + pad > n {
            return Err(RingError::Overrun);
        }
        let mut staged = core::mem::take(&mut self.staged);
        self.kind.nops(&mut staged, pad);
        for (i, &dw) in staged.iter().enumerate() {
            sink.put(self.wptr.wrapping_add(i as u64) & self.mask(), dw);
        }
        self.wptr = self.wptr.wrapping_add(staged.len() as u64);
        staged.clear();
        self.staged = staged;
        self.reserved = None;
        Ok(self.wptr)
    }

    /// Drops the open submission (amdgpu_ring.c:164 undo).
    pub fn undo(&mut self) {
        self.staged.clear();
        self.reserved = None;
    }

    /// An IB packet. SDMA: NOPs first so the 6-dword INDIRECT ends on an 8-dword boundary
    /// (sdma_v6_0.c:259 `(2 - wptr) & 7`); GFX: INDIRECT_BUFFER (or _CNST), dword-aligned address.
    pub fn emit_ib(&mut self, vmid: u8, addr: u64, len_dw: u32, csa: u64, cnst: bool) -> Result<(), RingError> {
        match self.kind {
            Kind::Sdma { .. } => {
                let pad = 2u64.wrapping_sub(self.pos()) & 7;
                let kind = self.kind;
                self.emit(|v| {
                    kind.nops(v, pad);
                    sdma::indirect(v, vmid, addr, len_dw, csa)
                })
            }
            Kind::Gfx => self.emit(|v| pm4::indirect_buffer(v, addr, len_dw, vmid, cnst, 0)),
        }
    }

    /// A fence writing `seq` to `addr` (GFX: RELEASE_MEM, 8-aligned when `wide`; SDMA: FENCE [+ FENCE] [+ TRAP]).
    pub fn emit_fence(&mut self, addr: u64, seq: u64, wide: bool, int: bool) -> Result<(), RingError> {
        match self.kind {
            Kind::Sdma { .. } => self.emit(|v| sdma::emit_fence(v, addr, seq, wide, int)),
            Kind::Gfx => self.emit(|v| pm4::fence(v, addr, seq, wide, int)),
        }
    }
}
