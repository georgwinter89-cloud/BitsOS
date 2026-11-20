//! FS-W2: the disk path as the kernel runs it, on the host: a volume on `CachedDiskDev` over `block::Disk` over a
//! block device that records what reaches it (`LogDisk`). The device has a volatile cache (a flush is its own
//! call), takes several calls at once like an NVMe controller, and can hold or fail the large writes (a file's
//! data: one request per megabyte) or the next write into a byte range. Its log has `dev::WriteLog`'s shape, so
//! the cut tests lay a power cut over it with `lay::cut_image`: every write before the last device flush is
//! stable, everything after it may be lost piece by piece.

use super::util::*;
use crate::dev::{CachedDiskDev, WriteLog, FLUSHED};
use crate::Volume;
use block::{Dispatch, Disk, DiskKind};
use devmodel::block::{BlockDevice, BlockInfo, IoPriority};
use devmodel::DevError;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::vec::Vec;

const SECTOR: usize = 512;
/// A write of at least this much is a file's data (metadata and the log go in runs of a few blocks).
pub const BIG: usize = 128 << 10;

/// What the device does with its large writes.
pub const PASS: u32 = 0;
pub const FAIL_IO: u32 = 1;
pub const FAIL_TIMEOUT: u32 = 2;

pub struct LogDisk {
    st: Mutex<(Vec<u8>, Option<WriteLog>)>,
    /// While shut, large writes wait in the device before they land.
    pub shut: AtomicBool,
    /// Large writes in the device now, and the most at once.
    pub inside: AtomicU32,
    pub peak: AtomicU32,
    /// `PASS`, `FAIL_IO` or `FAIL_TIMEOUT` for large writes.
    pub big: AtomicU32,
    /// The next write into bytes [start, end) waits while `hold_shut` (one write: the second goes through).
    hold: Mutex<Option<(u64, u64)>>,
    pub hold_shut: AtomicBool,
    pub held: AtomicU32,
    /// The next write into bytes [start, end) times out (one write).
    late: Mutex<Option<(u64, u64)>>,
    pub flushes: AtomicU64,
    pub big_bytes: AtomicU64,
}

impl LogDisk {
    pub fn new(img: Vec<u8>) -> Arc<LogDisk> {
        Arc::new(LogDisk {
            st: Mutex::new((img, None)),
            shut: AtomicBool::new(false),
            inside: AtomicU32::new(0),
            peak: AtomicU32::new(0),
            big: AtomicU32::new(PASS),
            hold: Mutex::new(None),
            hold_shut: AtomicBool::new(false),
            held: AtomicU32::new(0),
            late: Mutex::new(None),
            flushes: AtomicU64::new(0),
            big_bytes: AtomicU64::new(0),
        })
    }
    pub fn record(&self) {
        self.st.lock().unwrap().1 = Some(Vec::new());
    }
    /// Entries in the log now (a later cut index: what was logged before is before it).
    pub fn log_len(&self) -> usize {
        self.st.lock().unwrap().1.as_ref().map_or(0, |l| l.len())
    }
    pub fn take_log(&self) -> WriteLog {
        self.st.lock().unwrap().1.take().unwrap_or_default()
    }
    pub fn snapshot(&self) -> Vec<u8> {
        self.st.lock().unwrap().0.clone()
    }
    /// The next write into [start, end) is held until `hold_shut` is cleared.
    pub fn hold_next(&self, start: u64, end: u64) {
        self.hold_shut.store(true, Ordering::Release);
        *self.hold.lock().unwrap() = Some((start, end));
    }
    /// The next write into [start, end) is answered with a timeout and does not land.
    pub fn time_out_next(&self, start: u64, end: u64) {
        *self.late.lock().unwrap() = Some((start, end));
    }
    pub fn wait_inside(&self, n: u32) {
        wait("large writes at the device", || self.inside.load(Ordering::Acquire) >= n);
    }
}

pub fn wait(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("never: {what}");
}

impl BlockDevice for LogDisk {
    fn name(&self) -> &str {
        "fsw2log"
    }
    fn info(&self) -> BlockInfo {
        BlockInfo { block_size: SECTOR as u32, blocks: (self.st.lock().unwrap().0.len() / SECTOR) as u64, read_only: false, max_blocks_per_request: 2048, supports_discard: false, has_volatile_cache: true }
    }
    fn read_blocks(&self, lba: u64, buf: &mut [u8], _prio: IoPriority) -> Result<(), DevError> {
        let st = self.st.lock().unwrap();
        let at = lba as usize * SECTOR;
        buf.copy_from_slice(st.0.get(at..at + buf.len()).ok_or(DevError::InvalidArgument)?);
        Ok(())
    }
    fn write_blocks(&self, lba: u64, buf: &[u8], _fua: bool, _prio: IoPriority) -> Result<(), DevError> {
        let at = lba * SECTOR as u64;
        if buf.len() >= BIG {
            let now = self.inside.fetch_add(1, Ordering::AcqRel) + 1;
            self.peak.fetch_max(now, Ordering::AcqRel);
            while self.shut.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.inside.fetch_sub(1, Ordering::AcqRel);
            match self.big.load(Ordering::Acquire) {
                FAIL_IO => return Err(DevError::Io),
                FAIL_TIMEOUT => return Err(DevError::Timeout),
                _ => {}
            }
            self.big_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
        }
        {
            let mut l = self.late.lock().unwrap();
            if l.is_some_and(|(s, e)| at < e && at + buf.len() as u64 > s) {
                *l = None;
                return Err(DevError::Timeout);
            }
        }
        let mine = {
            let mut h = self.hold.lock().unwrap();
            let hit = h.is_some_and(|(s, e)| at < e && at + buf.len() as u64 > s);
            if hit {
                *h = None;
            }
            hit
        };
        if mine {
            self.held.fetch_add(1, Ordering::AcqRel);
            while self.hold_shut.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        let mut st = self.st.lock().unwrap();
        let a = at as usize;
        st.0.get_mut(a..a + buf.len()).ok_or(DevError::InvalidArgument)?.copy_from_slice(buf);
        if let Some(l) = st.1.as_mut() {
            l.push((at, buf.to_vec()));
        }
        Ok(())
    }
    fn flush(&self) -> Result<(), DevError> {
        self.flushes.fetch_add(1, Ordering::Relaxed);
        if let Some(l) = self.st.lock().unwrap().1.as_mut() {
            l.push((FLUSHED, Vec::new()));
        }
        Ok(())
    }
    fn queue_depth(&self) -> u32 {
        32
    }
}

/// Opens the gates when the test ends, also on a failed assert (the held threads would hang the run).
pub struct Open(pub Arc<LogDisk>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.shut.store(false, Ordering::Release);
        self.0.hold_shut.store(false, Ordering::Release);
    }
}

/// The device stack over `img`: the recording device, and ext4's device on it (the whole disk is the volume).
pub fn stack(img: Vec<u8>) -> (Arc<LogDisk>, Arc<CachedDiskDev>) {
    let blocks = (img.len() / SECTOR) as u64;
    let ld = LogDisk::new(img);
    let disk = Disk::open(ld.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    (ld, Arc::new(CachedDiskDev::new(disk, 0, blocks)))
}

/// A fresh volume on the disk path.
pub fn disk_volume(s: &Scratch, size: &str, layout: &[&str]) -> (Arc<LogDisk>, Arc<CachedDiskDev>, Arc<Volume>) {
    let (ld, dev) = stack(mkfs(s, size, layout, None));
    let v = Volume::mount(dev.clone(), opts()).expect("mount");
    (ld, dev, v)
}

/// The same behind the VFS at /data: its files' write-back sends their pages themselves (`Dev::write_pages`).
pub fn disk_ns(s: &Scratch, size: &str, layout: &[&str]) -> (Arc<LogDisk>, Arc<CachedDiskDev>, vfs::MountNs, Arc<Volume>, Arc<vfs::mount_ns::Mount>) {
    let (ld, dev) = stack(mkfs(s, size, layout, None));
    let ns = vfs::MountNs::new_empty();
    let (v, m) = crate::mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    (ld, dev, ns, v, m)
}

/// The volume's disk blocks of file `ino`, in file order.
pub fn blocks_of(v: &Volume, ino: u32) -> Vec<u64> {
    let di = v.stat(ino).unwrap();
    v.st.lock().extents(&di).unwrap().iter().flat_map(|e| e.pblk..e.pblk + e.len as u64).collect()
}
