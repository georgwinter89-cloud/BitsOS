//! GX-D3: the buffer-object manager over the UMA carve-out ("VRAM"; 8 GB at GPU 0x80_0000_0000 on the Mini-PC).
//! Handles, size, placement, reference counts, mmap offsets; reserved ranges (the GOP framebuffer, the DMCUB's
//! windows, the IP discovery blob) are never handed out. Scanout buffers are plain BOs: GX-3 adopts the GOP
//! framebuffer as BO "gop" and creates its back buffer at the place the attach gates checked (`create_at`).
//! GEM handles and dma-buf export map onto this 1:1 later (design: plan/notes/gx-GX-2.md §GX-D3).
//!
//! Pure bookkeeping (no hardware, no locks): the device object holds one manager under its own lock.
//! No fixed counts (R1): BTreeMaps, u32 handles and u64 offsets that only grow.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;

/// Linux reserves the top 64 KiB of VRAM for the IP discovery blob (`amdgpu_discovery.h:28`
/// DISCOVERY_TMR_OFFSET, `amdgpu_ttm.c:1733`); the blob itself is the first 8 KiB of it.
pub const DISCOVERY_RESERVE: u64 = 64 << 10;
pub const PAGE: u64 = 4096;
/// The first fake mmap offset (as DRM's `DRM_FILE_PAGE_OFFSET_START`: above anything a file offset means).
pub const MMAP_BASE: u64 = 1 << 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The carve-out, GPU address = FB base + offset.
    Vram,
    /// System memory through GART (GX-D7, MM-5b): not built yet.
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reserved {
    pub name: String,
    /// Carve-out offsets.
    pub range: Range<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bo {
    pub handle: u32,
    pub size: u64,
    pub placement: Placement,
    /// Carve-out offsets (Vram).
    pub range: Range<u64>,
    pub refs: u32,
    pub mmap_offset: u64,
    /// Adopted from a reserved range (the GOP framebuffer): freeing it gives the range back to the reserved list,
    /// never to the allocator.
    pub adopted: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoError {
    ZeroSize,
    /// No free range of that size and alignment (inside the CPU-visible limit, if one was asked).
    NoSpace,
    BadAlign,
    NoHandle,
    /// `create_at` over a reserved or used range, or outside the carve-out.
    Busy,
    PlacementUnsupported,
    NoSuchReserved,
    /// The fake mmap offset space is used up (never in practice: 2^64 bytes of offsets).
    NoMmapSpace,
}

#[derive(Debug)]
pub struct BoManager {
    size: u64,
    reserved: Vec<Reserved>,
    /// Free ranges: start -> end (exclusive), coalesced.
    free: BTreeMap<u64, u64>,
    bos: BTreeMap<u32, Bo>,
    next_handle: u32,
    next_mmap: u64,
}

impl BoManager {
    /// A manager over `size` bytes of carve-out with `reserved` ranges kept out (clipped to the carve-out).
    pub fn new(size: u64, reserved: Vec<Reserved>) -> Self {
        let mut m = BoManager {
            size,
            reserved: Vec::new(),
            free: BTreeMap::new(),
            bos: BTreeMap::new(),
            next_handle: 1,
            next_mmap: MMAP_BASE,
        };
        if size > 0 {
            m.free.insert(0, size);
        }
        for r in reserved {
            let range = r.range.start.min(size)..r.range.end.min(size);
            if !range.is_empty() {
                m.take(range.clone());
            }
            m.reserved.push(Reserved { name: r.name, range });
        }
        m
    }

    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn reserved(&self) -> &[Reserved] {
        &self.reserved
    }
    pub fn free_bytes(&self) -> u64 {
        self.free.iter().map(|(s, e)| e - s).sum()
    }

    /// Removes `r` from the free list (it must be free).
    fn take(&mut self, r: Range<u64>) {
        let hits: Vec<(u64, u64)> =
            self.free.range(..r.end).filter(|&(_, &e)| e > r.start).map(|(&s, &e)| (s, e)).collect();
        for (s, e) in hits {
            self.free.remove(&s);
            if s < r.start {
                self.free.insert(s, r.start);
            }
            if e > r.end {
                self.free.insert(r.end, e);
            }
        }
    }

    fn give(&mut self, r: Range<u64>) {
        let (mut s, mut e) = (r.start, r.end);
        if let Some((&ps, &pe)) = self.free.range(..s).next_back()
            && pe == s
        {
            self.free.remove(&ps);
            s = ps;
        }
        if let Some(&ne) = self.free.get(&e) {
            self.free.remove(&e);
            e = ne;
        }
        self.free.insert(s, e);
    }

    fn is_free(&self, r: &Range<u64>) -> bool {
        self.free.range(..=r.start).next_back().is_some_and(|(&s, &e)| s <= r.start && r.end <= e)
    }

    /// The handle and mmap offset the next BO of `size` bytes gets, checked before anything changes (so a
    /// failure never leaves a taken range behind).
    fn next_ids(&self, size: u64) -> Result<(u32, u64, u64), BoError> {
        let next_handle = self.next_handle.checked_add(1).ok_or(BoError::NoHandle)?;
        let span = size.checked_next_multiple_of(PAGE).ok_or(BoError::NoMmapSpace)?;
        let next_mmap = self.next_mmap.checked_add(span).ok_or(BoError::NoMmapSpace)?;
        Ok((next_handle, self.next_mmap, next_mmap))
    }

    fn insert(
        &mut self,
        size: u64,
        placement: Placement,
        range: Range<u64>,
        adopted: Option<String>,
        ids: (u32, u64, u64),
    ) -> u32 {
        let (next_handle, mmap_offset, next_mmap) = ids;
        let handle = self.next_handle;
        self.next_handle = next_handle;
        self.next_mmap = next_mmap;
        self.bos.insert(handle, Bo { handle, size, placement, range, refs: 1, mmap_offset, adopted });
        handle
    }

    /// A new BO, first fit at `align` (a power of two, at least a page); `limit`: it must end below this offset
    /// (the CPU-visible part: what BAR0 and the bridges reach).
    pub fn create(&mut self, size: u64, placement: Placement, align: u64, limit: Option<u64>) -> Result<u32, BoError> {
        if size == 0 {
            return Err(BoError::ZeroSize);
        }
        if placement != Placement::Vram {
            return Err(BoError::PlacementUnsupported);
        }
        let align = align.max(PAGE);
        if !align.is_power_of_two() {
            return Err(BoError::BadAlign);
        }
        let size = size.div_ceil(PAGE).checked_mul(PAGE).ok_or(BoError::NoSpace)?;
        let end_max = limit.unwrap_or(self.size).min(self.size);
        let at = self.free.iter().find_map(|(&s, &e)| {
            let a = s.checked_add(align - 1)? & !(align - 1);
            let end = a.checked_add(size)?;
            (end <= e && end <= end_max).then_some(a)
        });
        let a = at.ok_or(BoError::NoSpace)?;
        let ids = self.next_ids(size)?;
        self.take(a..a + size);
        Ok(self.insert(size, placement, a..a + size, None, ids))
    }

    /// A BO at a fixed place (GX-3's back buffer where the gates checked it).
    pub fn create_at(&mut self, range: Range<u64>) -> Result<u32, BoError> {
        if range.is_empty() || range.start % PAGE != 0 {
            return Err(BoError::BadAlign);
        }
        // Outside the carve-out first: an end near u64::MAX must not reach the rounding.
        if range.end > self.size {
            return Err(BoError::Busy);
        }
        let end = range.end.checked_next_multiple_of(PAGE).ok_or(BoError::Busy)?;
        let range = range.start..end;
        if range.end > self.size || !self.is_free(&range) {
            return Err(BoError::Busy);
        }
        let ids = self.next_ids(range.end - range.start)?;
        self.take(range.clone());
        Ok(self.insert(range.end - range.start, Placement::Vram, range, None, ids))
    }

    /// The reserved range `name` as a BO (the GOP framebuffer the firmware scans out).
    pub fn adopt(&mut self, name: &str) -> Result<u32, BoError> {
        let r = self.reserved.iter().find(|r| r.name == name).ok_or(BoError::NoSuchReserved)?.range.clone();
        if self.bos.values().any(|b| b.adopted.as_deref() == Some(name)) {
            return Err(BoError::Busy);
        }
        let ids = self.next_ids(r.end - r.start)?;
        Ok(self.insert(r.end - r.start, Placement::Vram, r, Some(String::from(name)), ids))
    }

    pub fn get(&self, handle: u32) -> Option<&Bo> {
        self.bos.get(&handle)
    }

    pub fn reference(&mut self, handle: u32) -> Option<u32> {
        let b = self.bos.get_mut(&handle)?;
        b.refs = b.refs.checked_add(1)?;
        Some(b.refs)
    }

    /// Drops one reference; the BO is gone (its range free again, unless adopted) at zero. Returns the refs left.
    pub fn release(&mut self, handle: u32) -> Option<u32> {
        let b = self.bos.get_mut(&handle)?;
        b.refs -= 1;
        let left = b.refs;
        if left == 0
            && let Some(b) = self.bos.remove(&handle)
            && b.adopted.is_none()
        {
            self.give(b.range);
        }
        Some(left)
    }

    /// The BO an mmap offset falls in (as `drm_vma_offset_lookup`).
    pub fn by_mmap(&self, offset: u64) -> Option<&Bo> {
        self.bos.values().find(|b| {
            let span = b.size.checked_next_multiple_of(PAGE).unwrap_or(u64::MAX);
            b.mmap_offset <= offset && offset < b.mmap_offset.saturating_add(span)
        })
    }

    /// Tests: start the fake mmap offsets near the end of their space.
    #[cfg(test)]
    pub fn set_next_mmap(&mut self, at: u64) {
        self.next_mmap = at;
    }

    pub fn count(&self) -> usize {
        self.bos.len()
    }
}

/// The reserved ranges of the carve-out from what the attach found: the GOP framebuffer (A0), the DMCUB's
/// windows that lie in graphics memory, and the discovery blob's top 64 KiB.
pub fn reserved_from(a: &crate::attach::Attach) -> Vec<Reserved> {
    let size = (a.fb_last - a.fb_first).saturating_add(1);
    let mut v = alloc::vec![Reserved { name: String::from("gop"), range: a.a0_offset..a.a0_offset + a.bytes }];
    for w in &a.windows {
        if w.last < a.fb_first || w.first > a.fb_last {
            continue;
        }
        let s = w.first.max(a.fb_first) - a.fb_first;
        let e = w.last.min(a.fb_last) - a.fb_first + 1;
        v.push(Reserved { name: alloc::format!("dmcub {}", w.name), range: s..e });
    }
    let mem = (u64::from(a.memsize_mb) << 20).min(size);
    v.push(Reserved { name: String::from("discovery"), range: mem.saturating_sub(DISCOVERY_RESERVE)..mem });
    v
}
