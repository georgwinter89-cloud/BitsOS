//! Transfer buffers (`usb_core::Buffer`) made by a controller: one or more DMA pieces within its address limit.
//! A class driver only reads and writes them; the controller takes the pieces' device addresses only from this
//! type (it refuses every other `XferMem`), so safe code cannot aim a transfer at memory it does not own.
//! A large buffer is one piece when contiguous memory is there and falls back to 64 KiB pieces otherwise; the
//! TD builder (`queue.rs`) cuts TRBs at every 64 KiB boundary either way.

use crate::hc::{Controller, Env};
use crate::mem::DmaMem;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, Ordering};
use usb_core::{Buffer, UsbError, XferMem};

/// Size of the pieces a large buffer falls back to (a TRB's most, xHCI 4.11.7.1).
pub const PIECE: usize = 0x1_0000;

pub struct XhciBuf<M: DmaMem> {
    pieces: Vec<M>,
    len: usize,
    /// A cancel could not stop the endpoint: the controller may still use this memory, so it is not handed out
    /// for another transfer until that TD is retired.
    pub(crate) stuck: Arc<AtomicBool>,
}

impl<M: DmaMem> XhciBuf<M> {
    /// (device address, length) of each piece, in order.
    pub fn segments(&self) -> Vec<(u64, usize)> {
        let mut left = self.len;
        self.pieces.iter().map(|p| {
            let n = p.size().min(left);
            left -= n;
            (p.dev_addr(), n)
        }).collect()
    }
    pub fn pieces(&self) -> usize {
        self.pieces.len()
    }
    pub fn is_stuck(&self) -> bool {
        self.stuck.load(Ordering::Acquire)
    }

    /// Calls `f(piece, offset in piece, range in the caller's slice)` for the bytes `off..off+n`.
    fn walk(&self, off: usize, n: usize, mut f: impl FnMut(&M, usize, core::ops::Range<usize>) -> bool) -> bool {
        let Some(end) = off.checked_add(n).filter(|&e| e <= self.len) else { return false };
        let (mut base, mut done) = (0usize, 0usize);
        for p in &self.pieces {
            let (lo, hi) = (base, base + p.size());
            base = hi;
            let (a, b) = (lo.max(off + done), hi.min(end));
            if a >= b {
                continue;
            }
            if !f(p, a - lo, done..done + (b - a)) {
                return false;
            }
            done += b - a;
            if off + done == end {
                break;
            }
        }
        off + done == end
    }
}

impl<M: DmaMem + 'static> XferMem for XhciBuf<M> {
    fn len(&self) -> usize {
        self.len
    }
    fn read(&self, off: usize, out: &mut [u8]) -> bool {
        self.walk(off, out.len(), |p, o, r| p.read_bytes(o, &mut out[r]))
    }
    fn write(&self, off: usize, data: &[u8]) -> bool {
        self.walk(off, data.len(), |p, o, r| p.write_bytes(o, &data[r]))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl<E: Env> Controller<E> {
    /// Zeroed transfer memory within this controller's limit (64-bit with AC64, else below 4 GiB): one piece if
    /// possible, else `PIECE`-sized pieces. `force_pieces` (host tests) skips the contiguous try.
    pub(crate) fn alloc_xfer_buffer(&self, len: usize, force_pieces: bool) -> Result<Buffer, UsbError> {
        let limit = if self.caps.ac64 { u64::MAX } else { 1 << 32 };
        let len_alloc = len.max(1);
        // A length from a device or a program must not panic the kernel: one beyond what the address space or
        // the controller's reach can hold is refused, the list of pieces is reserved fallibly, and a failing
        // piece frees those before it (dropping `v`).
        if len_alloc > isize::MAX as usize || len_alloc as u64 > limit {
            return Err(UsbError::Invalid("transfer buffer larger than the controller can reach"));
        }
        let one = if force_pieces || len_alloc <= PIECE { None } else { self.env.alloc_data(len_alloc, limit) };
        let pieces = match one {
            Some(m) => alloc::vec![m],
            None => {
                let mut v = Vec::new();
                v.try_reserve_exact(len_alloc.div_ceil(PIECE)).map_err(|_| UsbError::NoMemory)?;
                let mut left = len_alloc;
                while left > 0 {
                    let n = left.min(PIECE);
                    v.push(self.env.alloc_data(n, limit).ok_or(UsbError::NoMemory)?);
                    left -= n;
                }
                v
            }
        };
        if !pieces.iter().all(|p| p.zero()) {
            return Err(UsbError::NoMemory);
        }
        Ok(Arc::new(XhciBuf { pieces, len, stuck: Arc::new(AtomicBool::new(false)) }))
    }

    /// This controller's own buffer type behind a `Buffer`, its pieces inside the controller's address limit.
    pub(crate) fn own_buffer<'a>(&self, b: &'a Buffer) -> Result<&'a XhciBuf<E::Mem>, UsbError> {
        let x = b.as_any().downcast_ref::<XhciBuf<E::Mem>>().ok_or(UsbError::Invalid("a buffer not made by this host"))?;
        let limit = if self.caps.ac64 { u64::MAX } else { 1 << 32 };
        if x.segments().iter().any(|&(a, n)| a.checked_add(n as u64).is_none_or(|e| e > limit)) {
            return Err(UsbError::Invalid("buffer outside this controller's DMA limit"));
        }
        Ok(x)
    }
}
