//! `C: BlockDevice` (trait owner FS; back ends US (USB storage), NV (NVMe), VS (bitsvol), KS (crypt), FS (RAM
//! disk)). Devices stack: partition -> crypt -> bitsvol -> file system (B1). Every request carries a priority
//! (B4). Writes may be refused by the device's write gate (R8a: NVMe without `nvme_write=on`, a disk with a
//! Windows or EFI System partition).
//!
//! BL-2: a request may carry **frames** instead of a byte slice (`IoBuf` over `Frames`: page-cache pages, Linux's
//! bio_vec). Every layer passes the same frames down with its own block address (a partition adds its offset, a
//! volume maps its chunks and hands each piece on as a window of them), so the bottom device moves the bytes
//! between the medium and the frames itself (NVMe by DMA). A device that does not know frames gets them through
//! the default methods, which copy once through a bounce buffer (`block.copy_bytes.frames_bounce`).

use crate::DevError;
use alloc::sync::Arc;
use alloc::vec::Vec;

klog::counter!(FRAMES_BOUNCE, "block.copy_bytes.frames_bounce");
klog::counter!(FRAMES_BOUNCE_SHRUNK, "block.frames_bounce_shrunk");

/// Largest bounce buffer of the default frame methods (the device splits a larger transfer itself).
const BOUNCE_MAX: usize = 1 << 20;
/// Zeros for `IoBuf::zero` (holes of a thin volume).
static ZEROS: [u8; 4096] = [0; 4096];

/// I/O priority (B4): interactive reads first, background writeback last; aging prevents starvation (FS-12).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum IoPriority {
    Interactive,
    #[default]
    Normal,
    Background,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BlockInfo {
    /// Bytes per logical block (512 or 4096).
    pub block_size: u32,
    pub blocks: u64,
    pub read_only: bool,
    /// Largest single request in blocks (USB stick 090C:1000: 64).
    pub max_blocks_per_request: u32,
    pub supports_discard: bool,
    pub has_volatile_cache: bool,
}

/// BL-2: the memory of one request, seen as one run of `len()` bytes (page-cache pages, or the members of a
/// merged batch one after another). Implementors keep that memory alive as long as they live; the block layer
/// holds them by `Arc` until the device answered, so a caller that gives up (deadline) never leaves a device
/// writing into memory that was given back.
pub trait Frames: Send + Sync {
    fn len(&self) -> usize;
    /// The physically contiguous piece at byte `at` (`at < len()`): its device address (physical, the kernel has
    /// no IOMMU) and its bytes up to the end of the piece. None: the device cannot reach it (heap memory, host
    /// tests); it copies through `read`/`write` then.
    fn phys(&self, at: usize) -> Option<(u64, usize)>;
    /// Copies bytes `[at, at + out.len())` into `out`. False: outside (nothing copied).
    fn read(&self, at: usize, out: &mut [u8]) -> bool;
    /// Copies `src` to bytes `[at, at + src.len())`. False: outside (nothing copied).
    fn write(&self, at: usize, src: &[u8]) -> bool;
}

/// A window `[at, at + len)` of a request's frames. Cloning shares the frames (no copy); `slice` cuts a window
/// of it, checked against hostile lengths.
#[derive(Clone)]
pub struct IoBuf {
    frames: Arc<dyn Frames>,
    at: usize,
    len: usize,
}

impl IoBuf {
    pub fn new(frames: Arc<dyn Frames>) -> IoBuf {
        let len = frames.len();
        IoBuf { frames, at: 0, len }
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Bytes `[off, off + len)` of this window. None: not inside it.
    pub fn slice(&self, off: usize, len: usize) -> Option<IoBuf> {
        let end = off.checked_add(len)?;
        (end <= self.len).then(|| IoBuf { frames: self.frames.clone(), at: self.at + off, len })
    }
    /// The physically contiguous piece at `off` of this window, cut at the window's end (`Frames::phys`).
    pub fn phys(&self, off: usize) -> Option<(u64, usize)> {
        if off >= self.len {
            return None;
        }
        let (a, n) = self.frames.phys(self.at + off)?;
        (n > 0).then_some((a, n.min(self.len - off)))
    }
    fn inside(&self, off: usize, n: usize) -> bool {
        off.checked_add(n).is_some_and(|e| e <= self.len)
    }
    /// Copies `out.len()` bytes from `off` of the window. False: outside the window.
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        self.inside(off, out.len()) && self.frames.read(self.at + off, out)
    }
    /// Copies `src` to `off` of the window. False: outside the window.
    pub fn write(&self, off: usize, src: &[u8]) -> bool {
        self.inside(off, src.len()) && self.frames.write(self.at + off, src)
    }
    /// Zeroes `len` bytes from `off`. False: outside the window.
    pub fn zero(&self, off: usize, len: usize) -> bool {
        if !self.inside(off, len) {
            return false;
        }
        let mut done = 0;
        while done < len {
            let n = (len - done).min(ZEROS.len());
            if !self.frames.write(self.at + off + done, &ZEROS[..n]) {
                return false;
            }
            done += n;
        }
        true
    }
    /// Both windows are the same bytes of the same frames (tests: a layer passed the request down unchanged).
    pub fn same(&self, o: &IoBuf) -> bool {
        Arc::ptr_eq(&self.frames, &o.frames) && self.at == o.at && self.len == o.len
    }
}

/// One read of `buf.len()` bytes from block `lba` into frames (`BlockDevice::read_frames`).
#[derive(Clone)]
pub struct FrameIo {
    pub lba: u64,
    pub buf: IoBuf,
}

/// A bounce buffer of at most `want` bytes in whole blocks, halving while the heap cannot give it (a machine
/// whose free memory is in pieces still gets one block).
fn bounce(want: usize, bs: usize) -> Result<Vec<u8>, DevError> {
    let mut n = want.min(BOUNCE_MAX).max(bs) / bs * bs;
    loop {
        let mut v = Vec::new();
        if v.try_reserve_exact(n).is_ok() {
            v.resize(n, 0);
            return Ok(v);
        }
        if n <= bs {
            return Err(DevError::NoMemory);
        }
        n = (n / 2).max(bs) / bs * bs;
        FRAMES_BOUNCE_SHRUNK.inc();
    }
}

/// The default `read_frames`: each read through `read_blocks` into a bounce buffer, then into the frames.
pub fn bounce_read<D: BlockDevice + ?Sized>(d: &D, reqs: &[FrameIo], prio: IoPriority) -> Result<(), DevError> {
    let info = d.info();
    let bs = info.block_size.max(1) as usize;
    let max = (info.max_blocks_per_request.max(1) as usize).saturating_mul(bs);
    for r in reqs {
        if r.buf.is_empty() || !r.buf.len().is_multiple_of(bs) {
            return Err(DevError::InvalidArgument);
        }
        let mut b = bounce(r.buf.len().min(max), bs)?;
        let mut done = 0usize;
        while done < r.buf.len() {
            let n = (r.buf.len() - done).min(b.len());
            let lba = r.lba.checked_add((done / bs) as u64).ok_or(DevError::InvalidArgument)?;
            d.read_blocks(lba, &mut b[..n], prio)?;
            if !r.buf.write(done, &b[..n]) {
                return Err(DevError::InvalidArgument);
            }
            FRAMES_BOUNCE.add(n as u64);
            done += n;
        }
    }
    Ok(())
}

/// The default `write_frames`: the frames gathered into a bounce buffer, written through `write_blocks`.
pub fn bounce_write<D: BlockDevice + ?Sized>(d: &D, lba: u64, buf: &IoBuf, fua: bool, prio: IoPriority) -> Result<(), DevError> {
    let info = d.info();
    let bs = info.block_size.max(1) as usize;
    if buf.is_empty() || !buf.len().is_multiple_of(bs) {
        return Err(DevError::InvalidArgument);
    }
    let max = (info.max_blocks_per_request.max(1) as usize).saturating_mul(bs);
    let mut b = bounce(buf.len().min(max), bs)?;
    let mut done = 0usize;
    while done < buf.len() {
        let n = (buf.len() - done).min(b.len());
        if !buf.read(done, &mut b[..n]) {
            return Err(DevError::InvalidArgument);
        }
        let at = lba.checked_add((done / bs) as u64).ok_or(DevError::InvalidArgument)?;
        d.write_blocks(at, &b[..n], fua, prio)?;
        done += n;
    }
    Ok(())
}

/// BL-2: every call returns within the device's own bounded deadline (its command timeouts and recovery; Linux's
/// request timeout, `blk_mq_rq_timed_out`). The block layer calls it in the submitter's thread, without a
/// dispatcher thread whose waiter could give up first: an implementor whose call can wait without a bound breaks
/// B4. A stacked device meets it by going to its lower `Disk`, whose waits have deadlines.
pub trait BlockDevice: Send + Sync {
    fn name(&self) -> &str;
    fn info(&self) -> BlockInfo;
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError>;
    /// `fua`: the data is durable when this returns (journal commits).
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError>;
    /// Makes all completed writes durable (SYNCHRONIZE CACHE / NVMe Flush).
    fn flush(&self) -> Result<(), DevError>;
    /// Every flush this device answered before this call is durable when it returns (INT-W12, VS-2b CR 3: a
    /// stacked device may answer a flush early and make it durable later, bitsvol during a snapshot's freeze).
    /// Called on the fsync caller's thread, never from the block dispatcher. Default: flushes are durable when
    /// answered.
    fn settle(&self) -> Result<(), DevError> {
        Ok(())
    }
    /// Tells the device the blocks are unused (TRIM). Default: ignored.
    fn discard(&self, _lba: u64, _blocks: u64) -> Result<(), DevError> {
        Ok(())
    }
    /// BL-2: reads every `reqs[i]` (`buf.len()` bytes from `lba`, whole blocks) into its frames; a device may run
    /// them at once and in any order. Default: one after another through a bounce buffer (`bounce_read`).
    fn read_frames(&self, reqs: &[FrameIo], prio: IoPriority) -> Result<(), DevError> {
        bounce_read(self, reqs, prio)
    }
    /// BL-2: writes `buf` at `lba` from its frames (`fua` as `write_blocks`). Default: gathered into a bounce
    /// buffer, then `write_blocks` (`bounce_write`).
    fn write_frames(&self, lba: u64, buf: &IoBuf, fua: bool, prio: IoPriority) -> Result<(), DevError> {
        bounce_write(self, lba, buf, fua, prio)
    }
    /// BL-2: how many calls the device serves at once (an NVMe controller: the tags of all its queues). The
    /// block layer lets that many submitters call it together. Default: one at a time.
    fn queue_depth(&self) -> u32 {
        1
    }
}

/// Optional capability of a block device that can take consistent point-in-time snapshots (VS, B1).
pub trait Snapshots: Send + Sync {
    fn snapshot(&self, label: &str) -> Result<u64, DevError>;
    fn list(&self) -> alloc::vec::Vec<(u64, u64, alloc::string::String)>;
}
