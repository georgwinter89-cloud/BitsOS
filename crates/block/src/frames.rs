//! BL-2: the frames a request carries (`devmodel::block::Frames`, Linux's bio_vec list): page-cache pages as one
//! run of bytes (`PageFrames`), and the members of a merged batch one after another (`Concat`). The device reads
//! straight into them (NVMe by DMA at `phys`); nothing here copies unless a device asks through `read`/`write`.
//!
//! Lookups are by binary search over the segments' end offsets, so a device walking a 1 MiB request piece by
//! piece costs O(pieces x log segments).

use alloc::sync::Arc;
use alloc::vec::Vec;
use devmodel::block::{Frames, IoBuf};
use vfs::pagecache::CachePage;

/// Bytes of one page-cache page.
pub const PAGE: usize = vfs::frame::PAGE;

/// Part of a page-cache page: bytes `[off, off + len)` of `page`.
#[derive(Clone)]
pub struct PageSeg {
    pub page: Arc<CachePage>,
    pub off: usize,
    pub len: usize,
}

/// Index of the piece holding byte `at`, given the pieces' end offsets (ascending, last = total length).
fn find(ends: &[usize], at: usize) -> Option<usize> {
    let i = ends.partition_point(|&e| e <= at);
    (i < ends.len()).then_some(i)
}

/// Runs `f(piece, offset in piece, range in the caller's buffer)` over the pieces touching `[at, at + n)`.
/// False: the range is not inside `ends.last()`.
fn walk(ends: &[usize], at: usize, n: usize, mut f: impl FnMut(usize, usize, core::ops::Range<usize>) -> bool) -> bool {
    let Some(end) = at.checked_add(n) else { return false };
    if end > ends.last().copied().unwrap_or(0) {
        return false;
    }
    let mut pos = at;
    while pos < end {
        let Some(i) = find(ends, pos) else { return false };
        let start = if i == 0 { 0 } else { ends[i - 1] };
        let take = ends[i].min(end) - pos;
        if !f(i, pos - start, pos - at..pos - at + take) {
            return false;
        }
        pos += take;
    }
    true
}

/// Page-cache pages as one request's memory.
pub struct PageFrames {
    segs: Vec<PageSeg>,
    ends: Vec<usize>,
}

impl PageFrames {
    /// None: a segment is empty or not inside its page, or no memory for the index.
    pub fn new(segs: Vec<PageSeg>) -> Option<PageFrames> {
        let mut ends = Vec::new();
        ends.try_reserve_exact(segs.len()).ok()?;
        let mut total = 0usize;
        for s in &segs {
            if s.len == 0 || s.off.checked_add(s.len).is_none_or(|e| e > PAGE) {
                return None;
            }
            total = total.checked_add(s.len)?;
            ends.push(total);
        }
        Some(PageFrames { segs, ends })
    }

    pub fn segs(&self) -> &[PageSeg] {
        &self.segs
    }
}

impl Frames for PageFrames {
    fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }
    fn phys(&self, at: usize) -> Option<(u64, usize)> {
        let i = find(&self.ends, at)?;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        let s = &self.segs[i];
        let into = at - start;
        let base = s.page.phys()?;
        Some((base + (s.off + into) as u64, s.len - into))
    }
    fn read(&self, at: usize, out: &mut [u8]) -> bool {
        walk(&self.ends, at, out.len(), |i, o, r| {
            let s = &self.segs[i];
            s.page.read(s.off + o, &mut out[r]);
            true
        })
    }
    fn write(&self, at: usize, src: &[u8]) -> bool {
        walk(&self.ends, at, src.len(), |i, o, r| {
            let s = &self.segs[i];
            s.page.write(s.off + o, &src[r]);
            true
        })
    }
}

/// The members of a merged batch, in LBA order, as one request's memory (the device sees one transfer).
pub struct Concat {
    parts: Vec<IoBuf>,
    ends: Vec<usize>,
}

impl Concat {
    /// None: no memory for the index, or the lengths overflow.
    pub fn new(parts: Vec<IoBuf>) -> Option<Concat> {
        let mut ends = Vec::new();
        ends.try_reserve_exact(parts.len()).ok()?;
        let mut total = 0usize;
        for p in &parts {
            total = total.checked_add(p.len())?;
            ends.push(total);
        }
        Some(Concat { parts, ends })
    }
}

impl Frames for Concat {
    fn len(&self) -> usize {
        self.ends.last().copied().unwrap_or(0)
    }
    fn phys(&self, at: usize) -> Option<(u64, usize)> {
        let i = find(&self.ends, at)?;
        let start = if i == 0 { 0 } else { self.ends[i - 1] };
        self.parts[i].phys(at - start)
    }
    fn read(&self, at: usize, out: &mut [u8]) -> bool {
        walk(&self.ends, at, out.len(), |i, o, r| self.parts[i].read(o, &mut out[r]))
    }
    fn write(&self, at: usize, src: &[u8]) -> bool {
        walk(&self.ends, at, src.len(), |i, o, r| self.parts[i].write(o, &src[r]))
    }
}

/// Plain bytes as frames (host tests and the copy path of a byte request merged with frames): never reachable
/// by DMA.
pub struct HeapFrames(pub ksync::SpinLock<Vec<u8>>);

impl HeapFrames {
    pub fn new(v: Vec<u8>) -> HeapFrames {
        HeapFrames(ksync::SpinLock::named("block.heap_frames", v))
    }
    pub fn take(&self) -> Vec<u8> {
        core::mem::take(&mut *self.0.lock())
    }
}

impl Frames for HeapFrames {
    fn len(&self) -> usize {
        self.0.lock().len()
    }
    fn phys(&self, _at: usize) -> Option<(u64, usize)> {
        None
    }
    fn read(&self, at: usize, out: &mut [u8]) -> bool {
        let g = self.0.lock();
        match at.checked_add(out.len()).and_then(|e| g.get(at..e)) {
            Some(s) => {
                out.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    fn write(&self, at: usize, src: &[u8]) -> bool {
        let mut g = self.0.lock();
        match at.checked_add(src.len()).and_then(|e| g.get_mut(at..e)) {
            Some(s) => {
                s.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
}
