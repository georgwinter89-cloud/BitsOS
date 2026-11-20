//! BIGDIR test helpers: a memory device that counts its reads and can fail one, names like Steam's.

use super::util::opts;
use crate::dev::{Dev, PageRead};
use crate::{MemDev, Volume};
use std::format;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::vec::Vec;

/// A memory device that counts reads and can fail the n-th read or write from now.
pub struct CountDev {
    pub inner: Arc<MemDev>,
    pub reads: AtomicU64,
    fail_read_in: AtomicI64,
    fail_write_in: AtomicI64,
}

impl CountDev {
    pub fn new(img: Vec<u8>) -> Arc<CountDev> {
        Arc::new(CountDev { inner: Arc::new(MemDev::new(img)), reads: AtomicU64::new(0), fail_read_in: AtomicI64::new(0), fail_write_in: AtomicI64::new(0) })
    }
    pub fn reads(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }
    /// The `n`-th read from now fails once (0: none).
    pub fn fail_read(&self, n: i64) {
        self.fail_read_in.store(n, Ordering::Relaxed);
    }
    /// The `n`-th write from now fails once (0: none).
    pub fn fail_write(&self, n: i64) {
        self.fail_write_in.store(n, Ordering::Relaxed);
    }
    /// Reads still to come before the armed one fails (0: it failed, or none is armed).
    pub fn fail_read_left(&self) -> i64 {
        self.fail_read_in.load(Ordering::Relaxed).max(0)
    }
    fn due(c: &AtomicI64) -> bool {
        c.load(Ordering::Relaxed) > 0 && c.fetch_sub(1, Ordering::Relaxed) == 1
    }
}

impl Dev for CountDev {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read(&self, at: u64, buf: &mut [u8]) -> Result<(), i64> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if Self::due(&self.fail_read_in) {
            return Err(crate::errno::EIO);
        }
        self.inner.read(at, buf)
    }
    fn write(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        if Self::due(&self.fail_write_in) {
            return Err(crate::errno::EIO);
        }
        self.inner.write(at, data)
    }
    fn flush(&self) -> Result<(), i64> {
        self.inner.flush()
    }
    fn write_through(&self, at: u64, data: &[u8]) -> Result<(), i64> {
        if Self::due(&self.fail_write_in) {
            return Err(crate::errno::EIO);
        }
        self.inner.write_through(at, data)
    }
    fn read_pages(&self, reads: Vec<PageRead>) -> Result<(), i64> {
        self.inner.read_pages(reads)
    }
}

pub fn mount_counted(img: Vec<u8>) -> (Arc<CountDev>, Arc<Volume>) {
    let dev = CountDev::new(img);
    let v = Volume::mount(dev.clone(), opts()).expect("mount");
    (dev, v)
}

/// Names like Steam's: 30 to 60 bytes, mixed case.
pub fn steam_name(i: usize, tag: &str) -> Vec<u8> {
    let pad = &"SharedData_Texture_Atlas_Block"[..i % 31];
    format!("Civ7_{tag}{i:06}_Base-Platforms{pad}.blp").into_bytes()
}
