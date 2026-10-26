//! fbdev's memory: ordinary RAM pages (write-back), never the GOP BAR (V2-PLAN §2.8). Allocated as MM-4 DMA
//! buffers of 2 MiB each (a buddy block of order 9 is easy to find even late; one 29 MB run might not be), handed
//! to MM-5b as the apertures of the device mapping, and read by the kernel row by row into the picture.

use alloc::sync::Arc;
use alloc::vec::Vec;
use mm::device::Aperture;
use mm::dma::{DmaBuffer, DmaConstraints, DmaError};

/// Bytes per piece (a multiple of the page size and of 4, so a pixel never straddles two pieces).
pub const CHUNK: usize = 2 << 20;
const PAGE: usize = 4096;

pub struct FbMem {
    chunks: Vec<Arc<DmaBuffer>>,
    len: usize,
}

impl FbMem {
    /// `len` zeroed bytes (rounded up to whole pages) in pieces of `CHUNK`.
    pub fn alloc(len: usize) -> Result<FbMem, DmaError> {
        let len = len.div_ceil(PAGE) * PAGE;
        if len == 0 {
            return Err(DmaError::Invalid);
        }
        let mut chunks = Vec::new();
        chunks.try_reserve_exact(len.div_ceil(CHUNK)).map_err(|_| DmaError::NoMemory)?;
        let mut done = 0;
        while done < len {
            let n = (len - done).min(CHUNK);
            chunks.push(Arc::new(DmaBuffer::alloc(n, DmaConstraints::dma64())?));
            done += n;
        }
        Ok(FbMem { chunks, len })
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn pages(&self) -> u64 {
        (self.len / PAGE) as u64
    }
    /// The pieces as MM apertures (index = piece).
    pub fn apertures(&self) -> Vec<Aperture> {
        self.chunks.iter().map(|c| Aperture::Ram(c.clone())).collect()
    }
    /// Page `pgoff` of the buffer as (piece, page in it).
    pub fn locate(&self, pgoff: u64) -> Option<(usize, u64)> {
        if pgoff >= self.pages() {
            return None;
        }
        let per = (CHUNK / PAGE) as u64;
        Some(((pgoff / per) as usize, pgoff % per))
    }

    /// Walks `[off, off + n)` piece by piece: `f(piece, offset in it, offset in the walk, length)`; false if the
    /// range leaves the buffer (nothing is visited then) or `f` says false.
    fn walk(&self, off: usize, n: usize, mut f: impl FnMut(&DmaBuffer, usize, usize, usize) -> bool) -> bool {
        if off.checked_add(n).is_none_or(|e| e > self.len) {
            return false;
        }
        let mut done = 0;
        while done < n {
            let at = off + done;
            let (c, o) = (at / CHUNK, at % CHUNK);
            let k = (CHUNK - o).min(n - done);
            if !f(&self.chunks[c], o, done, k) {
                return false;
            }
            done += k;
        }
        true
    }

    /// Copies bytes `off..off + out.len()` out (read(2) on /dev/fb0).
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        let n = out.len();
        self.walk(off, n, |c, o, d, k| c.copy_out(o, &mut out[d..d + k]))
    }
    /// Copies `data` in at `off` (write(2) on /dev/fb0).
    pub fn write(&self, off: usize, data: &[u8]) -> bool {
        self.walk(off, data.len(), |c, o, d, k| c.copy_in(o, &data[d..d + k]))
    }
    /// Copies the 32-bit pixels at byte `off` (a multiple of 4) into `dst` (the picture's rows).
    pub fn read_pixels(&self, off: usize, dst: &mut [u32]) -> bool {
        if !off.is_multiple_of(4) {
            return false;
        }
        let Some(n) = dst.len().checked_mul(4) else { return false };
        self.walk(off, n, |c, o, d, k| hal::drv_dp::copy_from_region_u32(c.cpu(), o, &mut dst[d / 4..(d + k) / 4]))
    }
}
