//! The RAM disk (`C: BlockDevice` stub since G0, FS-6's test device): any block size, a transfer limit to
//! test merging against, error injection per range (the in-kernel side of QEMU blkdebug's `inject-error`), a
//! pause to hold the device busy while a test queues requests, and a log of the calls the device got.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use devmodel::DevError;
use ksync::{SpinLock, WaitQueue};

/// What the device was asked to do (the call log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOp {
    Read,
    Write { fua: bool },
    Flush,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    pub op: CallOp,
    pub lba: u64,
    pub blocks: u64,
    pub prio: IoPriority,
}

/// An injected error: calls of `op` (Read/Write; `Write` matches any FUA flag) touching `[lba, lba + blocks)`
/// fail with `error`, `times` more times (None: always).
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub write: bool,
    pub lba: u64,
    pub blocks: u64,
    pub error: DevError,
    pub times: Option<u32>,
}

/// Longest a paused call waits for `resume` (a forgotten pause must not hang the dispatcher, B4).
const PAUSE_LIMIT_NS: u64 = 10_000_000_000;

pub struct RamDisk {
    name: String,
    block_size: u32,
    data: SpinLock<Vec<u8>>,
    read_only: bool,
    max_blocks: AtomicU64,
    faults: SpinLock<Vec<Fault>>,
    log: SpinLock<Option<Vec<Call>>>,
    paused: AtomicBool,
    /// A call is waiting inside the pause.
    held: AtomicBool,
    pause_wq: WaitQueue,
    flushes: AtomicU64,
    /// FS-12: every device call takes this long (a slow disk for the hog tests; 0 = none).
    delay_ns: AtomicU64,
}

impl RamDisk {
    /// Panics only if the heap cannot hold the disk (tests; use `try_new` for a size from elsewhere).
    pub fn new(name: &str, block_size: u32, blocks: u64) -> Self {
        Self::try_new(name, block_size, blocks).expect("RamDisk::new: no memory")
    }

    pub fn try_new(name: &str, block_size: u32, blocks: u64) -> Result<Self, DevError> {
        if block_size == 0 {
            return Err(DevError::InvalidArgument);
        }
        let bytes = (block_size as u64).checked_mul(blocks).and_then(|b| usize::try_from(b).ok()).ok_or(DevError::InvalidArgument)?;
        let mut v = Vec::new();
        v.try_reserve_exact(bytes).map_err(|_| DevError::NoMemory)?;
        v.resize(bytes, 0);
        Ok(Self::from_image(name, block_size, v, false))
    }

    pub fn from_image(name: &str, block_size: u32, image: Vec<u8>, read_only: bool) -> Self {
        RamDisk {
            name: String::from(name),
            block_size: block_size.max(1),
            data: SpinLock::named("block.ram", image),
            read_only,
            max_blocks: AtomicU64::new(u32::MAX as u64),
            faults: SpinLock::named("block.ram.faults", Vec::new()),
            log: SpinLock::named("block.ram.log", None),
            paused: AtomicBool::new(false),
            held: AtomicBool::new(false),
            pause_wq: WaitQueue::new(),
            flushes: AtomicU64::new(0),
            delay_ns: AtomicU64::new(0),
        }
    }

    /// The transfer limit the disk reports (`max_blocks_per_request`).
    pub fn with_max_blocks(self, n: u32) -> Self {
        self.max_blocks.store(n.max(1) as u64, Ordering::Relaxed);
        self
    }

    pub fn inject(&self, f: Fault) {
        self.faults.lock().push(f);
    }

    pub fn clear_faults(&self) {
        self.faults.lock().clear();
    }

    /// Starts (or restarts) the call log.
    pub fn record(&self) {
        *self.log.lock() = Some(Vec::new());
    }

    pub fn calls(&self) -> Vec<Call> {
        self.log.lock().clone().unwrap_or_default()
    }

    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }

    /// FS-12: every device call from now on sleeps `ns` after it was recorded (capped at 100 ms per call).
    pub fn set_delay_ns(&self, ns: u64) {
        self.delay_ns.store(ns.min(100_000_000), Ordering::Release);
    }

    /// Device calls from now on wait (at most 10 s) until `resume`.
    pub fn pause(&self) {
        self.paused.store(true, Ordering::Release);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::Release);
        self.pause_wq.wake_all();
    }

    /// A call is waiting in the pause (the device is "busy").
    pub fn holding(&self) -> bool {
        self.held.load(Ordering::Acquire)
    }

    /// A copy of the bytes (tests).
    pub fn snapshot(&self) -> Vec<u8> {
        self.data.lock().clone()
    }

    /// Overwrites bytes at `off` without a request (tests prepare an image).
    pub fn poke(&self, off: usize, b: &[u8]) {
        let mut d = self.data.lock();
        if let Some(s) = off.checked_add(b.len()).and_then(|e| d.get_mut(off..e)) {
            s.copy_from_slice(b);
        }
    }

    fn enter(&self, op: CallOp, lba: u64, blocks: u64, prio: IoPriority) -> Result<(), DevError> {
        if self.paused.load(Ordering::Acquire) {
            self.held.store(true, Ordering::Release);
            self.pause_wq.wait_until_timeout(|| !self.paused.load(Ordering::Acquire), PAUSE_LIMIT_NS);
            self.held.store(false, Ordering::Release);
        }
        if let Some(l) = self.log.lock().as_mut() {
            l.push(Call { op, lba, blocks, prio });
        }
        let delay = self.delay_ns.load(Ordering::Acquire);
        if delay > 0 {
            sched::sleep_ns(delay);
        }
        let write = match op {
            CallOp::Read => false,
            CallOp::Write { .. } => true,
            _ => return Ok(()),
        };
        let mut faults = self.faults.lock();
        let hit = faults.iter().position(|f| f.write == write && f.lba < lba + blocks && lba < f.lba + f.blocks);
        if let Some(i) = hit {
            let e = faults[i].error;
            if let Some(t) = faults[i].times.as_mut() {
                *t = t.saturating_sub(1);
                if *t == 0 {
                    faults.remove(i);
                }
            }
            return Err(e);
        }
        Ok(())
    }

    fn range(&self, lba: u64, len: usize, total: usize) -> Result<core::ops::Range<usize>, DevError> {
        if !len.is_multiple_of(self.block_size as usize) {
            return Err(DevError::InvalidArgument);
        }
        let start = lba.checked_mul(self.block_size as u64).and_then(|s| usize::try_from(s).ok()).ok_or(DevError::InvalidArgument)?;
        let end = start.checked_add(len).ok_or(DevError::InvalidArgument)?;
        if end > total {
            return Err(DevError::InvalidArgument);
        }
        Ok(start..end)
    }
}

impl BlockDevice for RamDisk {
    fn name(&self) -> &str {
        &self.name
    }
    fn info(&self) -> BlockInfo {
        BlockInfo {
            block_size: self.block_size,
            blocks: self.data.lock().len() as u64 / self.block_size as u64,
            read_only: self.read_only,
            max_blocks_per_request: self.max_blocks.load(Ordering::Relaxed) as u32,
            supports_discard: true,
            has_volatile_cache: false,
        }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], prio: IoPriority) -> Result<(), DevError> {
        self.enter(CallOp::Read, lba, (buf.len() / self.block_size as usize) as u64, prio)?;
        let d = self.data.lock();
        let r = self.range(lba, buf.len(), d.len())?;
        buf.copy_from_slice(&d[r]);
        Ok(())
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], fua: bool, prio: IoPriority) -> Result<(), DevError> {
        if self.read_only {
            return Err(DevError::Refused);
        }
        self.enter(CallOp::Write { fua }, lba, (buf.len() / self.block_size as usize) as u64, prio)?;
        let mut d = self.data.lock();
        let r = self.range(lba, buf.len(), d.len())?;
        d[r].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&self) -> Result<(), DevError> {
        self.enter(CallOp::Flush, 0, 0, IoPriority::Normal)?;
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn discard(&self, lba: u64, blocks: u64) -> Result<(), DevError> {
        if self.read_only {
            return Err(DevError::Refused);
        }
        self.enter(CallOp::Discard, lba, blocks, IoPriority::Background)?;
        let bs = self.block_size as usize;
        let mut d = self.data.lock();
        let len = usize::try_from(blocks).ok().and_then(|b| b.checked_mul(bs)).ok_or(DevError::InvalidArgument)?;
        let r = self.range(lba, len, d.len())?;
        d[r].fill(0);
        Ok(())
    }
}
