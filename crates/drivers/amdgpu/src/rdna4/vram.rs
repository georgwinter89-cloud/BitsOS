//! GP-GPU2: where the card's PSP buffers go in VRAM; wave 42: `Bump`, the one manager of the card's VRAM in a boot
//! (the picture, the PSP's window and the firmware's own region are kept from it; the GART table, the default page
//! and later rings and buffers come from what is left).
//!
//! Linux places the PSP's ring, fence, command and private buffers as kernel BOs that need CPU access, so bottom-up in
//! visible VRAM (amdgpu_object.c:138-141), and GMC 12 reserves nothing at the bottom for the firmware's picture
//! (`gmc_v12_0_get_vbios_fb_size` returns 0, gmc_v12_0.c:551-554): Linux writes its first buffers over the firmware
//! framebuffer, which it has removed by then (amdgpu_device.c:3966-3971). The TOP of VRAM is not free: the firmware
//! reserves a region there whose size only the VBIOS knows (amdgpu_ttm.c:1741-1772, `fw_reserved_size_in_kb`) and
//! which the secure OS reports again (`psp_update_fw_reservation`, amdgpu_psp.c:1065-1120); on this card Linux's GART
//! table lands 517 MiB below the top, so about 516 MiB there are taken [U]. A top-down stub "below the discovery
//! reserve" would write into that region, the secure OS's own memory among it. So, decided: bottom-up as Linux, but
//! above the firmware framebuffer (the picture must stay), in a window that ends far below the top.
//!
//! Linux's `AMDGPU_DEBUG_USE_VRAM_FW_BUF` (amdgpu_drv.c:141, :2261-2263) puts the firmware buffer and the private
//! buffer in VRAM too (amdgpu_ucode.c:1153, amdgpu_psp.c:539-541), where its default puts them in GTT: that is the
//! layout taken here, so the PSP never reads system memory and no GART and no bus master are needed (G3's).

use super::attach::BarFact;
use crate::attach::Gop;
use crate::bo::{BoError, PAGE};
use crate::gpucore::psp::BoAlloc;
use alloc::vec::Vec;
use core::ops::Range;

/// The window's size: the 1 MiB firmware buffer, ring, fence and command pages, the firmware images (about 3 MiB
/// with the SMU's, VCN's and the display's) and the trusted applications' three 16 KiB buffers fit many times.
pub const WINDOW: u64 = 64 << 20;
/// Distance kept from the end of the firmware framebuffer.
pub const GUARD: u64 = 2 << 20;

/// Why no window was placed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VramStop {
    /// The window would reach past half of VRAM (where the firmware's own regions may begin).
    NoRoom { start: u64 },
}

/// VRAM as the PSP stages see it: MC address of offset 0, the firmware framebuffer (VRAM offsets, if it lies on this
/// card), and the window the buffers come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub vram_bytes: u64,
    pub mc_base: u64,
    pub fb: Option<Range<u64>>,
    pub window: Range<u64>,
}

impl Layout {
    pub fn mc(&self, offset: u64) -> u64 {
        self.mc_base + offset
    }

    /// The VRAM offset of an MC address inside VRAM.
    pub fn offset_of(&self, mc: u64, bytes: u64) -> Option<u64> {
        let off = mc.checked_sub(self.mc_base)?;
        (off.checked_add(bytes)? <= self.vram_bytes).then_some(off)
    }

    /// The window as MC addresses (what a stage may write to).
    pub fn window_mc(&self) -> Range<u64> {
        self.mc(self.window.start)..self.mc(self.window.end)
    }
}

/// The window for a VRAM of `vram_bytes` at `mc_base`, from the card's BAR0 and the firmware framebuffer
/// (`crate::gpu::gop`). Where VRAM lies is the `gmc` stage's to say (`gmcstage::plan` with `hub41::layout`: as
/// Linux from the location base and the memory size register alone, `mmhub_v4_1_0_get_fb_location`
/// mmhub_v4_1_0.c:516-531, gmc_v12_0.c:688-708 and :733-734; MMMC_VM_FB_LOCATION_TOP is never read), and what a
/// base that is no address stops with is that function's (`hub41::LayoutStop`).
pub fn place(vram_bytes: u64, mc_base: u64, bar0: BarFact, gop: Option<Gop>) -> Result<Layout, VramStop> {
    let fb = gop.filter(|g| g.bytes != 0 && g.phys >= bar0.phys).and_then(|g| {
        let start = g.phys - bar0.phys;
        let end = start.checked_add(g.bytes)?;
        (end <= vram_bytes.min(bar0.bytes)).then_some(start..end)
    });
    let start = fb.as_ref().map_or(0, |f| f.end).next_multiple_of(GUARD) + GUARD;
    let end = start + WINDOW;
    if end > vram_bytes / 2 {
        return Err(VramStop::NoRoom { start });
    }
    Ok(Layout { vram_bytes, mc_base, fb, window: start..end })
}

/// Why a part of VRAM is kept from the manager's free space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kept {
    /// The firmware's picture with the distance kept behind it.
    Picture,
    /// The secure processor's window: only its stages are handed parts of it (`BoAlloc`).
    PspWindow,
    /// The firmware's own region, as the secure OS names it after ring create (`psp_update_fw_reservation`,
    /// amdgpu_psp.c:1065-1120): Linux knows it from the VBIOS before it allocates anything, BitsOS only then.
    Firmware,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RangeStop {
    /// No free range of that size and alignment below the ceiling.
    NoRoom { what: &'static str, bytes: u64 },
    /// A reservation would lie over something already handed out or kept.
    Overlap { kept: Kept, range: Range<u64>, over: &'static str, at: Range<u64> },
    /// Outside VRAM, empty, or an alignment that is no power of two.
    Bad { what: &'static str },
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

/// The card's VRAM in this boot (VRAM offsets): three kinds of kept ranges and what was handed out. Nothing handed
/// out ever overlaps a kept range or another handed-out one.
#[derive(Clone, Debug)]
pub struct Bump {
    vram_bytes: u64,
    window: Range<u64>,
    next: u64,
    kept: Vec<(Kept, Range<u64>)>,
    /// What the secure processor's stages hold of the window, in order.
    pub given: Vec<Range<u64>>,
    /// What everything else holds, outside every kept range: (what, range).
    pub taken: Vec<(&'static str, Range<u64>)>,
}

impl Bump {
    pub fn new(l: &Layout) -> Bump {
        let mut kept = Vec::new();
        if let Some(fb) = &l.fb {
            kept.push((Kept::Picture, fb.start..l.window.start));
        }
        kept.push((Kept::PspWindow, l.window.clone()));
        Bump { vram_bytes: l.vram_bytes, next: l.window.start, window: l.window.clone(), kept, given: Vec::new(), taken: Vec::new() }
    }

    /// Takes back everything handed out of the window after the first `kept` buffers: for buffers the card was never
    /// told of (a ring whose registers did not go out).
    pub fn rewind(&mut self, kept: usize) {
        if let Some(first) = self.given.get(kept) {
            self.next = first.start;
            self.given.truncate(kept);
        }
    }

    /// Whether `bytes` at the VRAM offset `at` lie inside what the secure processor's stages were handed.
    pub fn holds(&self, at: u64, bytes: u64) -> bool {
        self.given.iter().any(|r| at >= r.start && at.saturating_add(bytes) <= r.end)
    }

    pub fn kept(&self) -> &[(Kept, Range<u64>)] {
        &self.kept
    }

    /// Until the firmware's region is known nothing is handed out above half of VRAM: Linux saw that region at the
    /// top (about 516 MiB on this card), and it must not be met by guessing.
    fn ceiling(&self) -> u64 {
        if self.kept.iter().any(|(k, _)| *k == Kept::Firmware) { self.vram_bytes } else { self.vram_bytes / 2 }
    }

    /// A free range outside every kept range, lowest first (the GART table, the default page, rings, buffers).
    pub fn take(&mut self, what: &'static str, bytes: u64, align: u64) -> Result<Range<u64>, RangeStop> {
        if bytes == 0 || !align.is_power_of_two() {
            return Err(RangeStop::Bad { what });
        }
        let (bytes, align) = (bytes.next_multiple_of(PAGE), align.max(PAGE));
        let mut busy: Vec<&Range<u64>> = self.kept.iter().map(|(_, r)| r).chain(self.taken.iter().map(|(_, r)| r)).collect();
        busy.sort_by_key(|r| r.start);
        let mut at = 0u64;
        for b in busy {
            if at.checked_add(bytes).is_some_and(|e| e <= b.start) {
                break;
            }
            at = at.max(b.end).next_multiple_of(align);
        }
        match at.checked_add(bytes) {
            Some(end) if end <= self.ceiling() => {
                self.taken.push((what, at..end));
                Ok(at..end)
            }
            _ => Err(RangeStop::NoRoom { what, bytes }),
        }
    }

    /// Gives a taken range back; false when it was not taken as a whole.
    pub fn give_back(&mut self, range: &Range<u64>) -> bool {
        let before = self.taken.len();
        self.taken.retain(|(_, r)| r != range);
        self.taken.len() != before
    }

    /// Why the firmware's region could not be kept here, if it could not.
    fn firmware_clash(&self, range: &Range<u64>) -> Option<RangeStop> {
        if range.start >= range.end || range.end > self.vram_bytes {
            return Some(RangeStop::Bad { what: "the firmware's region" });
        }
        let clash = |over: &'static str, at: &Range<u64>| RangeStop::Overlap { kept: Kept::Firmware, range: range.clone(), over, at: at.clone() };
        if let Some((_, r)) = self.kept.iter().find(|(k, r)| *k != Kept::Firmware && overlap(r, range)) {
            return Some(clash("a kept range", r));
        }
        self.taken.iter().find(|(_, r)| overlap(r, range)).map(|(what, r)| clash(what, r))
    }

    /// Keeps the firmware's regions (the secure OS names two), all of them or none: refused, and nothing changed,
    /// when anything handed out or kept already lies in one of them. The caller then stops (the ring stage
    /// destroys its ring), and the ceiling stays where it was.
    pub fn keep_firmware(&mut self, ranges: &[Range<u64>]) -> Result<(), RangeStop> {
        if let Some(stop) = ranges.iter().find_map(|r| self.firmware_clash(r)) {
            return Err(stop);
        }
        for range in ranges {
            if !self.kept.iter().any(|(k, r)| *k == Kept::Firmware && r == range) {
                self.kept.push((Kept::Firmware, range.clone()));
            }
        }
        Ok(())
    }

    /// The window comes back whole (the ring's destroy): a buffer the secure OS may still read is never handed out
    /// twice before that.
    pub fn window_back(&mut self) {
        self.given.clear();
        self.next = self.window.start;
    }
}

/// The secure processor's stages: upwards inside the window.
impl BoAlloc for Bump {
    fn alloc(&mut self, size: u64, align: u64) -> Result<(u32, Range<u64>), BoError> {
        let size = size.next_multiple_of(PAGE);
        let at = self.next.next_multiple_of(align.max(PAGE));
        let end = at.checked_add(size).filter(|e| *e <= self.window.end).ok_or(BoError::NoSpace)?;
        self.next = end;
        self.given.push(at..end);
        Ok((self.given.len() as u32, at..end))
    }

    /// Nothing is reused before the whole window goes back (`window_back`).
    fn release(&mut self, _handle: u32) {}
}
