//! What the FAT code reads and writes: a volume as sectors (sector 0 = its boot sector). In the kernel a
//! partition (or the whole disk) of a `block::Disk` (`stick::DiskDev`); in host tests an image in memory that
//! can record every write in order (the power-cut tests replay the first k writes) and fail on demand.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::*;

pub trait Dev: Send + Sync {
    /// Bytes per sector (the disk's block size).
    fn sector_size(&self) -> usize;
    /// Sectors of the volume.
    fn sectors(&self) -> u64;
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), i64>;
    fn write(&self, sector: u64, data: &[u8]) -> Result<(), i64>;
    /// The device's cache to stable storage (SYNCHRONIZE CACHE on a USB stick).
    fn flush(&self) -> Result<(), i64>;
    /// FS-12: where the volume lock's holder and waiters are noted for the request priorities (`lift`).
    fn lift(&self) -> Option<&crate::lift::Lift> {
        None
    }
}

/// Recorded writes: (sector, bytes), in order.
pub type WriteLog = Vec<(u64, Vec<u8>)>;

/// A volume image in memory.
pub struct MemDev {
    ss: usize,
    img: SpinLock<Vec<u8>>,
    log: SpinLock<Option<WriteLog>>,
    flushes: AtomicU64,
    fail_writes: AtomicBool,
    fail_reads: AtomicBool,
    /// Host tests: microseconds every read waits before it copies (widens race windows).
    slow_reads_us: AtomicU64,
    /// FS-14b: bytes read so far (the host tests' `usb.msd.read_bytes`).
    read_bytes: AtomicU64,
}

impl MemDev {
    pub fn new(img: Vec<u8>, ss: usize) -> MemDev {
        MemDev {
            ss,
            img: SpinLock::named("fat.memdev", img),
            log: SpinLock::named("fat.memdev.log", None),
            flushes: AtomicU64::new(0),
            fail_writes: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            slow_reads_us: AtomicU64::new(0),
            read_bytes: AtomicU64::new(0),
        }
    }
    /// Starts recording every write (sector, bytes) in order.
    pub fn record(&self) {
        *self.log.lock() = Some(Vec::new());
    }
    pub fn take_log(&self) -> WriteLog {
        self.log.lock().take().unwrap_or_default()
    }
    pub fn image(&self) -> Vec<u8> {
        self.img.lock().clone()
    }
    pub fn flushes(&self) -> u64 {
        self.flushes.load(Ordering::Relaxed)
    }
    pub fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::Relaxed);
    }
    pub fn fail_reads(&self, on: bool) {
        self.fail_reads.store(on, Ordering::Relaxed);
    }
    pub fn slow_reads(&self, us: u64) {
        self.slow_reads_us.store(us, Ordering::Relaxed);
    }
    /// Bytes read so far.
    pub fn read_bytes(&self) -> u64 {
        self.read_bytes.load(Ordering::Relaxed)
    }
}

impl Dev for MemDev {
    fn sector_size(&self) -> usize {
        self.ss
    }
    fn sectors(&self) -> u64 {
        (self.img.lock().len() / self.ss) as u64
    }
    fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), i64> {
        if self.fail_reads.load(Ordering::Relaxed) {
            return Err(EIO);
        }
        #[cfg(test)]
        match self.slow_reads_us.load(Ordering::Relaxed) {
            0 => {}
            us => std::thread::sleep(core::time::Duration::from_micros(us)),
        }
        let img = self.img.lock();
        let at = sector as usize * self.ss;
        let src = img.get(at..at + buf.len()).ok_or(EIO)?;
        buf.copy_from_slice(src);
        self.read_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        Ok(())
    }
    fn write(&self, sector: u64, data: &[u8]) -> Result<(), i64> {
        if self.fail_writes.load(Ordering::Relaxed) {
            return Err(EIO);
        }
        let at = sector as usize * self.ss;
        {
            let mut img = self.img.lock();
            let dst = img.get_mut(at..at + data.len()).ok_or(EIO)?;
            dst.copy_from_slice(data);
        }
        if let Some(l) = self.log.lock().as_mut() {
            l.push((sector, data.to_vec()));
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), i64> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
