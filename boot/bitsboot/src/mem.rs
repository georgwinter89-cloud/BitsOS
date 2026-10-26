//! Page allocation with BitsOS memory types and the merged RAM ranges of a memory map.

use crate::{Ctx, R};
use alloc::vec::Vec;
use bootinfo::{MemDesc, memtype};
use core::cell::UnsafeCell;
use uefi::boot::{self, AllocateType, MemoryType};
use uefi::mem::memory_map::{MemoryDescriptor, MemoryMap};

pub const PAGE: u64 = 4096;

pub fn pages_for(bytes: u64) -> u64 {
    bytes.div_ceil(PAGE).max(1)
}

/// Every block `alloc` handed out and not yet freed, so a failed boot can give all of them back before the
/// firmware starts the next boot entry (Windows must not inherit BitsOS memory types).
struct Blocks(UnsafeCell<Vec<(u64, u64)>>);

/// Blocks whose BitsOS type is given to the firmware as LOADER_DATA and put back into the map only after
/// ExitBootServices (`retyped`): the Mini-PC's firmware hung inside ExitBootServices (A0, 15.10.) while the
/// map held OS-defined types (0x8000_0000 and up), which UEFI allows but V1 and Linux never used.
struct Typed(UnsafeCell<Vec<(u64, u64, u32)>>);
// SAFETY: as `Blocks`.
unsafe impl Sync for Typed {}
static TYPED: Typed = Typed(UnsafeCell::new(Vec::new()));

fn typed() -> &'static mut Vec<(u64, u64, u32)> {
    // SAFETY: as `blocks`.
    unsafe { &mut *TYPED.0.get() }
}

/// How many BitsOS-typed blocks exist (each can split one firmware descriptor into up to three).
pub fn typed_blocks() -> usize {
    typed().len()
}
// SAFETY: the loader runs on one CPU at TPL_APPLICATION; the list is touched only from `alloc`/`free`.
unsafe impl Sync for Blocks {}
static BLOCKS: Blocks = Blocks(UnsafeCell::new(Vec::new()));

fn blocks() -> &'static mut Vec<(u64, u64)> {
    // SAFETY: see `Sync` above; no caller keeps the reference across another call.
    unsafe { &mut *BLOCKS.0.get() }
}

/// Allocates `pages` pages of the type `ty`; `what` names the purpose in the error message.
pub fn alloc(ty: u32, at: AllocateType, pages: u64, what: &str) -> R<u64> {
    let fw_ty = if ty >= 0x8000_0000 { memtype::LOADER_DATA } else { ty };
    let p = boot::allocate_pages(at, MemoryType(fw_ty), pages as usize)
        .ctx(&alloc::format!("not enough memory for {what} ({pages} pages)"))?;
    let phys = p.as_ptr() as u64;
    if blocks().try_reserve(1).is_ok() {
        blocks().push((phys, pages));
    }
    if fw_ty != ty {
        if typed().try_reserve(1).is_err() {
            free_pages(phys, pages);
            return Err(crate::fail!("no memory to record the type of {what}"));
        }
        typed().push((phys, pages, ty));
    }
    Ok(phys)
}

fn free_pages(phys: u64, pages: u64) {
    if let Some(p) = core::ptr::NonNull::new(phys as *mut u8) {
        // SAFETY: callers pass a block `alloc` returned that nothing refers to any more (UEFI FreePages rule).
        let _ = unsafe { boot::free_pages(p, pages as usize) };
    }
}

/// Gives back a block `alloc` returned (best effort: a failure only leaves memory the kernel frees anyway).
pub fn free(phys: u64, pages: u64) {
    blocks().retain(|&(p, _)| p != phys);
    typed().retain(|&(p, _, _)| p != phys);
    free_pages(phys, pages);
}

/// Gives back the block `alloc` returned at `phys`, with the page count recorded then.
pub fn free_block(phys: u64) {
    let pages = blocks().iter().find(|&&(p, _)| p == phys).map(|&(_, n)| n);
    if let Some(n) = pages {
        free(phys, n);
    }
}

/// The boot failed: give every block back to the firmware. Returns how many pages that were.
pub fn free_all() -> u64 {
    typed().clear();
    let list = core::mem::take(blocks());
    let total = list.iter().map(|&(_, n)| n).sum();
    for (phys, pages) in list {
        free_pages(phys, pages);
    }
    total
}

/// A physical range as a byte slice. Callers pass only ranges they allocated (or the firmware handed them)
/// and never hold two slices over the same bytes at once.
pub fn bytes(phys: u64, len: u64) -> &'static mut [u8] {
    // SAFETY: UEFI identity-maps memory on x86_64 (UEFI 2.10 §2.3.4), so the physical address is a valid
    // pointer for `len` bytes this loader owns; after ExitBootServices the same tables stay live until the jump.
    unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, len as usize) }
}

pub fn zero(phys: u64, len: u64) {
    bytes(phys, len).fill(0);
}

pub fn normalise(d: &MemoryDescriptor) -> MemDesc {
    MemDesc { ty: d.ty.0, phys: d.phys_start, virt: d.virt_start, pages: d.page_count, attr: d.att.bits() }
}

/// The final map for the kernel: each firmware descriptor, with the BitsOS-typed blocks inside it cut out
/// and given their BitsOS type again. Allocation free (runs after ExitBootServices); the map must be sorted.
pub fn retyped<'a, M: MemoryMap>(map: &'a M) -> Retyped<impl Iterator<Item = &'a MemoryDescriptor> + Clone + 'a> {
    Retyped { entries: map.entries(), cur: None }
}

#[derive(Clone)]
pub struct Retyped<I> {
    entries: I,
    cur: Option<MemDesc>,
}

impl<'a, I: Iterator<Item = &'a MemoryDescriptor>> Iterator for Retyped<I> {
    type Item = MemDesc;
    fn next(&mut self) -> Option<MemDesc> {
        let d = match self.cur.take() {
            Some(d) => d,
            None => normalise(self.entries.next()?),
        };
        let end = d.phys.saturating_add(d.pages.saturating_mul(PAGE));
        // The first typed block that starts inside this descriptor.
        let hit = typed().iter().filter(|&&(p, n, _)| p >= d.phys && p < end && n > 0).min_by_key(|&&(p, _, _)| p).copied();
        let Some((p, n, ty)) = hit else { return Some(d) };
        if p > d.phys {
            // The part before the block; the rest comes next round.
            self.cur = Some(MemDesc { phys: p, virt: p, pages: (end - p) / PAGE, ..d });
            return Some(MemDesc { pages: (p - d.phys) / PAGE, ..d });
        }
        let bend = p.saturating_add(n.saturating_mul(PAGE)).min(end);
        if bend < end {
            self.cur = Some(MemDesc { phys: bend, virt: bend, pages: (end - bend) / PAGE, ..d });
        }
        Some(MemDesc { ty, pages: (bend - p) / PAGE, ..d })
    }
}

/// Adjacent RAM descriptors of a map sorted by address, merged into `(start, end)` ranges. Allocation free,
/// so it also runs on the final map after ExitBootServices.
pub fn ram_ranges<'a>(map: &'a impl MemoryMap) -> impl Iterator<Item = (u64, u64)> + 'a {
    let mut it = map.entries().filter(|d| memtype::is_ram(d.ty.0) && d.page_count > 0).peekable();
    core::iter::from_fn(move || {
        let d = it.next()?;
        let (start, mut end) = (d.phys_start, d.phys_start.saturating_add(d.page_count.saturating_mul(PAGE)));
        while let Some(n) = it.peek() {
            if n.phys_start != end {
                break;
            }
            end = end.saturating_add(n.page_count.saturating_mul(PAGE));
            it.next();
        }
        Some((start, end))
    })
}
