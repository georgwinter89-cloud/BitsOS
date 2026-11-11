//! The few syscalls std does not offer: posix_fadvise, a read-only file mapping, mincore, and the clock ticks
//! of /proc/self/stat. Raw x86-64 syscalls, so the same static binary runs on V2 and on CachyOS.

use std::os::fd::AsRawFd;

const SYS_MMAP: usize = 9;
const SYS_MUNMAP: usize = 11;
const SYS_MINCORE: usize = 27;
const SYS_FADVISE64: usize = 221;
const PROT_READ: usize = 1;
const MAP_SHARED: usize = 1;
pub const POSIX_FADV_DONTNEED: usize = 4;
pub const PAGE: usize = 4096;

/// The `syscall` instruction with six arguments.
///
/// # Safety
/// Every argument the kernel treats as a pointer must point at memory the call may read or write, and the call
/// must not unmap or change memory Rust code still uses.
unsafe fn raw(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    // SAFETY: the instruction itself only enters the kernel and returns; the arguments are the caller's contract.
    unsafe {
        core::arch::asm!("syscall", inlateout("rax") n as isize => r,
            in("rdi") a[0], in("rsi") a[1], in("rdx") a[2], in("r10") a[3], in("r8") a[4], in("r9") a[5],
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    r
}

fn ret(r: isize) -> Result<usize, i32> {
    if (-4095..0).contains(&r) { Err(-r as i32) } else { Ok(r as usize) }
}

/// posix_fadvise(fd, off, len, advice); Err(errno).
pub fn fadvise(f: &std::fs::File, off: u64, len: u64, advice: usize) -> Result<(), i32> {
    // SAFETY: only integers are passed; no memory is touched.
    ret(unsafe { raw(SYS_FADVISE64, [f.as_raw_fd() as usize, off as usize, len as usize, advice, 0, 0]) }).map(|_| ())
}

/// A read-only shared mapping of a whole file; unmapped on drop.
pub struct FileMap {
    addr: usize,
    len: usize,
}

impl FileMap {
    pub fn new(f: &std::fs::File, len: usize) -> Result<FileMap, i32> {
        // SAFETY: no address hint, no MAP_FIXED: the kernel places the mapping where nothing is mapped.
        let a = ret(unsafe { raw(SYS_MMAP, [0, len, PROT_READ, MAP_SHARED, f.as_raw_fd() as usize, 0]) })?;
        Ok(FileMap { addr: a, len })
    }
    /// One byte per page from page `first` to `end`, in order (each a page fault on a cold file); their sum.
    pub fn touch(&self, first: usize, end: usize) -> u64 {
        let mut sum = 0u64;
        for p in first..end.min(self.len.div_ceil(PAGE)) {
            // SAFETY: inside our own read-only mapping; volatile so every page is really touched.
            sum += unsafe { core::ptr::read_volatile((self.addr + p * PAGE) as *const u8) } as u64;
        }
        sum
    }
    /// mincore of the whole mapping: the resident pages.
    pub fn resident(&self) -> Result<usize, i32> {
        let mut v = vec![0u8; self.len.div_ceil(PAGE)];
        // SAFETY: the kernel writes one byte per page of our own mapping into v, which is sized for it.
        ret(unsafe { raw(SYS_MINCORE, [self.addr, self.len, v.as_mut_ptr() as usize, 0, 0, 0]) })?;
        Ok(v.iter().filter(|&&b| b & 1 != 0).count())
    }
}

impl Drop for FileMap {
    fn drop(&mut self) {
        // SAFETY: our own whole mapping; nothing refers into it after this.
        unsafe { raw(SYS_MUNMAP, [self.addr, self.len, 0, 0, 0, 0]) };
    }
}

/// This process's (utime, stime) in clock ticks from /proc/self/stat (fields 14 and 15, after the comm's ')').
pub fn cpu_ticks() -> (u64, u64) {
    std::fs::read_to_string("/proc/self/stat").ok().and_then(|s| crate::bench::stat_ticks(&s)).unwrap_or((0, 0))
}
