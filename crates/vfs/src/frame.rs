//! Page memory for the page cache (FS-5): one 4 KiB frame from the frame allocator per cached page, so a file
//! page has a physical address that a file mapping (MM) and later the block layer (FS-6, DMA into cache pages)
//! can use. Safe code reaches the frame only through mm-core's `PageFrame` handle (bounds-checked copies, the
//! frame freed when the handle drops; FS-5 change request 3, MM-5b). FS-14b: the frames are accounted as
//! `Kind::PageCache` (before, through `dma::alloc`, as `Kind::Dma`: `mem.used.pagecache` always read 0).
//!
//! Without a frame allocator (host tests, never in the kernel after `mm_core` starts) the page lives in the
//! kernel heap and has no physical address; its "address" is then only an identity for tests.

use alloc::boxed::Box;
use alloc::vec::Vec;
use ksync::SpinLock;

pub const PAGE: usize = 4096;

enum Mem {
    Block(mm_core::PageFrame),
    Heap(SpinLock<Box<[u8]>>),
}

/// One zeroed page that this cache page owns.
pub struct Frame(Mem);

impl Frame {
    /// A zeroed frame; None when memory is out.
    pub fn new_zeroed() -> Option<Frame> {
        if mm_core::boot_buddy().is_some() {
            return mm_core::PageFrame::new_zeroed().map(|b| Frame(Mem::Block(b)));
        }
        let mut v: Vec<u8> = Vec::new();
        v.try_reserve_exact(PAGE).ok()?;
        v.resize(PAGE, 0);
        Some(Frame(Mem::Heap(SpinLock::named("vfs.frame.heap", v.into_boxed_slice()))))
    }

    /// The physical address (None for a heap page, which cannot be mapped into a program).
    pub fn phys(&self) -> Option<u64> {
        match &self.0 {
            Mem::Block(b) => Some(b.phys()),
            Mem::Heap(_) => None,
        }
    }

    /// A stable identity of this page: the physical address, or for a heap page its kernel address (tests).
    pub fn ident(&self) -> u64 {
        match &self.0 {
            Mem::Block(b) => b.phys(),
            Mem::Heap(h) => h.lock().as_ptr() as u64,
        }
    }

    /// Copies `out.len()` bytes from `off` (the caller keeps inside the page).
    pub fn read(&self, off: usize, out: &mut [u8]) {
        match &self.0 {
            Mem::Block(b) => {
                b.read(off, out);
            }
            Mem::Heap(h) => {
                let g = h.lock();
                if let Some(s) = g.get(off..off + out.len()) {
                    out.copy_from_slice(s);
                }
            }
        }
    }

    pub fn write(&self, off: usize, data: &[u8]) {
        match &self.0 {
            Mem::Block(b) => {
                b.write(off, data);
            }
            Mem::Heap(h) => {
                let mut g = h.lock();
                if let Some(s) = g.get_mut(off..off + data.len()) {
                    s.copy_from_slice(data);
                }
            }
        }
    }

    /// Zeroes [off, PAGE).
    pub fn zero_from(&self, off: usize) {
        self.zero_range(off, PAGE.saturating_sub(off));
    }

    /// Zeroes [off, off + len), clipped to the page (fallocate's hole punch on a partly covered page).
    pub fn zero_range(&self, off: usize, len: usize) {
        if off >= PAGE {
            return;
        }
        let len = len.min(PAGE - off);
        match &self.0 {
            Mem::Block(b) => {
                b.fill(off, len, 0);
            }
            Mem::Heap(h) => h.lock()[off..off + len].fill(0),
        }
    }
}
