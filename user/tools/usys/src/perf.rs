//! SELFOPT-1: what the sampling profiler `bitsprof` needs from the kernel: `perf_event_open`, the event ioctls,
//! the ring buffer as a shared mapping it owns, and a signalfd for SIGTERM/SIGINT (so the program needs no signal
//! handler). The ring's pages are written by the kernel at any time: they are only read volatile or copied as
//! bytes, never through a Rust reference.

use super::{Errno, SysResult, raw, ret};
use std::sync::atomic::{Ordering, fence};

const PERF_EVENT_OPEN: usize = 298;
const SIGNALFD4: usize = 289;
const RT_SIGPROCMASK: usize = 14;
const IOCTL: usize = 16;
const MMAP: usize = 9;
const MUNMAP: usize = 11;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;
const PAGE: usize = 4096;
/// `perf_event_mmap_page` offsets.
const DATA_HEAD: usize = 1024;
const DATA_TAIL: usize = 1032;

/// perf_event_open(attr, pid, cpu, group_fd, flags); `attr` is the whole attribute (its size word says how much
/// the kernel reads, and it must not say more than `attr.len()`).
pub fn perf_event_open(attr: &mut [u8], pid: i32, cpu: i32, group_fd: i32, flags: usize) -> SysResult {
    let size = attr.get(4..8).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    if attr.len() < 64 || size > attr.len() {
        return Err(Errno(22));
    }
    // SAFETY: the kernel reads `size` bytes of attr (at most its length, checked) and may write the size word
    // back (E2BIG); attr is borrowed mutably for the call.
    ret(unsafe { raw(PERF_EVENT_OPEN, [attr.as_mut_ptr() as usize, pid as isize as usize, cpu as isize as usize, group_fd as isize as usize, flags, 0]) })
}

/// An event ioctl whose argument is a plain number (ENABLE, DISABLE, RESET, REFRESH, SET_OUTPUT, PAUSE_OUTPUT).
pub fn perf_ioctl(fd: usize, cmd: usize, arg: usize) -> SysResult {
    // SAFETY: these commands take an integer argument; the kernel reads no memory through it.
    ret(unsafe { raw(IOCTL, [fd, cmd, arg, 0, 0, 0]) })
}

/// PERF_EVENT_IOC_ID: the event's id.
pub fn perf_id(fd: usize) -> SysResult<u64> {
    let mut v = 0u64;
    // SAFETY: the kernel writes one u64 into v.
    ret(unsafe { raw(IOCTL, [fd, 0x8008_2407, &mut v as *mut u64 as usize, 0, 0, 0]) }).map(|_| v)
}

/// Blocks `mask` (signal bits, bit n-1 for signal n) and returns a signalfd that reads them (O_CLOEXEC|O_NONBLOCK).
pub fn block_into_signalfd(mask: u64) -> SysResult {
    let m = mask;
    // SAFETY: the kernel reads the 8-byte set m; no old set is asked for.
    ret(unsafe { raw(RT_SIGPROCMASK, [0, &m as *const u64 as usize, 0, 8, 0, 0]) })?;
    // SAFETY: the kernel reads the 8-byte set m.
    ret(unsafe { raw(SIGNALFD4, [usize::MAX, &m as *const u64 as usize, 8, 0o2004000, 0, 0]) })
}

/// The ring buffer of one event: a control page and 2^n data pages, mapped shared and writable (the program
/// writes `data_tail`). Unmapped on drop.
pub struct PerfRing {
    addr: usize,
    len: usize,
    data: u64,
}

impl PerfRing {
    /// mmap of 1 + `data_pages` pages of event `fd`.
    pub fn map(fd: usize, data_pages: usize) -> SysResult<PerfRing> {
        let len = (1 + data_pages) * PAGE;
        // SAFETY: no address hint and no MAP_FIXED: the kernel places the mapping where nothing is mapped.
        let a = ret(unsafe { raw(MMAP, [0, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0]) })?;
        Ok(PerfRing { addr: a, len, data: (data_pages * PAGE) as u64 })
    }

    fn word(&self, off: usize) -> u64 {
        // SAFETY: inside the control page of our own mapping; volatile, the kernel changes it at any time.
        unsafe { core::ptr::read_volatile((self.addr + off) as *const u64) }
    }

    /// A word of the control page (`perf_event_mmap_page`), by byte offset.
    pub fn control(&self, off: usize) -> u64 {
        if off + 8 > PAGE { 0 } else { self.word(off) }
    }

    /// `data_head`, then a read barrier (the records before it are complete).
    pub fn head(&self) -> u64 {
        let h = self.word(DATA_HEAD);
        fence(Ordering::Acquire);
        h
    }

    pub fn tail(&self) -> u64 {
        self.word(DATA_TAIL)
    }

    /// Hands the bytes before `t` back to the kernel (a full barrier first: every read of them is done).
    pub fn set_tail(&self, t: u64) {
        fence(Ordering::SeqCst);
        // SAFETY: data_tail lies in the control page of our own mapping; the kernel only reads it.
        unsafe { core::ptr::write_volatile((self.addr + DATA_TAIL) as *mut u64, t) };
    }

    /// Bytes of the data area.
    pub fn data_size(&self) -> u64 {
        self.data
    }

    /// Copies the stream bytes `[pos, pos + out.len())` (wrap undone). Only bytes between tail and head are
    /// stable: the kernel does not write there until the tail moves past them.
    pub fn copy(&self, pos: u64, out: &mut [u8]) {
        if self.data == 0 || out.len() as u64 > self.data {
            return;
        }
        let off = (pos & (self.data - 1)) as usize;
        let first = out.len().min(self.data as usize - off);
        let base = self.addr + PAGE;
        // SAFETY: both pieces lie in the data area of our own mapping (off < data, first <= data - off, the rest
        // <= off); byte copies into our buffer, never a reference into the shared pages.
        unsafe {
            core::ptr::copy_nonoverlapping((base + off) as *const u8, out.as_mut_ptr(), first);
            core::ptr::copy_nonoverlapping(base as *const u8, out.as_mut_ptr().add(first), out.len() - first);
        }
    }
}

impl Drop for PerfRing {
    fn drop(&mut self) {
        // SAFETY: our own whole mapping; no Rust reference points into it.
        let _ = unsafe { raw(MUNMAP, [self.addr, self.len, 0, 0, 0, 0]) };
    }
}
