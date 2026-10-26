//! Changing the memory type of RAM in the direct map (MM-4: UC/WC DMA buffers), the way Linux's
//! `set_memory_uc`/`set_memory_wc` do it (`arch/x86/mm/pat/set_memory.c`): large leaves covering the range are
//! split down to 4 KiB leaves with the same frames and rights, then only the cache bits of the range's leaves
//! change. One memory type per frame is the SDM's rule (11.12.4): a UC/WC buffer must not stay reachable as WB
//! through the direct map, so the alias itself is retyped instead of a second mapping being added.
//!
//! Split leaves stay split after the type goes back to WB (Linux collapses them later; open point in
//! plan/notes/mm-MM-4.md): it costs TLB reach, not correctness.

use super::walk::{index, leaf_bits, leaf_flags, leaf_phys, span, Tables};
use super::{CacheType, MapError, PageFlags, ADDR_MASK};
use core::sync::atomic::{AtomicBool, Ordering};

/// What a retype did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetypeStats {
    pub split_1g: u64,
    pub split_2m: u64,
    pub pages: u64,
}

/// Splits every large leaf that maps a byte of [virt, virt + len) (4 KiB aligned) into 512 leaves of the next
/// size with the same frames and flags, until the range is mapped by 4 KiB leaves only. A table that runs out
/// of frames halfway leaves what was split split: the same mapping with smaller pages.
pub(super) fn split_range(t: &mut impl Tables, root: u64, virt: u64, len: u64, st: &mut RetypeStats) -> Result<(), MapError> {
    let mut va = virt;
    while va < virt + len {
        split_down(t, root, va, st)?;
        va += 0x1000;
    }
    Ok(())
}

fn split_down(t: &mut impl Tables, root: u64, va: u64, st: &mut RetypeStats) -> Result<(), MapError> {
    let mut table = root;
    for level in (2..=4).rev() {
        let i = index(va, level);
        let mut e = t.read(table, i);
        if e & PageFlags::PRESENT == 0 {
            return Err(MapError::NotMapped);
        }
        if level <= 3 && e & PageFlags::HUGE != 0 {
            let child = t.alloc_table().ok_or(MapError::NoMemory)?;
            let (flags, base, step) = (leaf_flags(e, level), leaf_phys(e, level), span(level - 1));
            for j in 0..512u64 {
                t.write(child, j as usize, (base + j * step) | leaf_bits(flags, level - 1));
            }
            // The leaves carry every right and NX; the new table entry only lets them through.
            e = child | PageFlags::PRESENT | PageFlags::WRITABLE | (flags & PageFlags::USER);
            t.write(table, i, e);
            if level == 3 {
                st.split_1g += 1;
            } else {
                st.split_2m += 1;
            }
        }
        table = e & ADDR_MASK;
    }
    Ok(())
}

/// The table and index of the present 4 KiB leaf for `va`, if that is how it is mapped.
fn leaf_slot(t: &impl Tables, root: u64, va: u64) -> Option<(u64, usize)> {
    let mut table = root;
    for level in (2..=4).rev() {
        let e = t.read(table, index(va, level));
        if e & PageFlags::PRESENT == 0 || e & PageFlags::HUGE != 0 {
            return None;
        }
        table = e & ADDR_MASK;
    }
    let i = index(va, 1);
    (t.read(table, i) & PageFlags::PRESENT != 0).then_some((table, i))
}

/// Sets the cache bits (`CacheType::bits`, 4 KiB layout) of every leaf in [virt, virt + len). Every page must
/// be a present 4 KiB leaf (checked for all before the first change); returns the pages changed.
pub(super) fn set_cache_range(t: &mut impl Tables, root: u64, virt: u64, len: u64, bits: u64) -> Result<u64, MapError> {
    let pages = len / 0x1000;
    for p in 0..pages {
        leaf_slot(t, root, virt + p * 0x1000).ok_or(MapError::NotMapped)?;
    }
    let mask = PageFlags::PWT | PageFlags::PCD | PageFlags::PAT_4K;
    for p in 0..pages {
        if let Some((table, i)) = leaf_slot(t, root, virt + p * 0x1000) {
            let e = t.read(table, i);
            t.write(table, i, (e & !mask) | (bits & mask));
        }
    }
    Ok(pages)
}

/// Serialises retypes: two splits of the same large leaf must not race. Taken only with interrupts off on
/// this CPU (`IrqGuard`), so no preemption or interrupt can park a holder and every wait is bounded (B4), the
/// same convention as `vectors::ensure_table`. The frame allocator's lock (an IRQ lock) nests inside it.
static BUSY: AtomicBool = AtomicBool::new(false);

/// Gives the direct-map pages of the RAM range [phys, phys + len) (4 KiB aligned) the memory type `cache` in
/// the kernel root (all user roots share its kernel half), splitting large leaves as needed, and flushes every
/// CPU's TLB. The caller owns the frames (a DMA buffer it just allocated or is about to free) and flushes their
/// cache lines afterwards. Refused before the kernel's own tables are live (`WrongRange`), for UC/WC when the
/// PAT does not hold Linux's layout (`Unsupported`: PWT alone would select WT, not WC), and for any page the
/// direct map does not map (`NotMapped`, nothing changed). With other CPUs online both flushes are shootdowns
/// (MM-6); `NeedsShootdown` when a CPU did not confirm one: after the split nothing else changed, after the type
/// change the frames carry the new type but a CPU may still hold the old one, so the caller must never hand
/// them out again.
///
/// Thread context only: it runs with interrupts off for the whole split (bounded: one walk per 4 KiB page),
/// and the table frames come from the frame allocator.
///
/// Two TLB flushes, as Linux's `__split_large_page` does: the split changes only page sizes (same frames,
/// same type), which the SDM allows to coexist in the TLB for a moment; the first flush retires the old large
/// entries before any type changes, because a large WB entry next to a 4 KiB UC/WC entry for the same address
/// is undefined behaviour (SDM 4.10.4.2; AMD erratum 383 is the real-world case).
pub fn set_direct_map_cache(phys: u64, len: u64, cache: CacheType) -> Result<RetypeStats, MapError> {
    if len == 0 || !phys.is_multiple_of(0x1000) || !len.is_multiple_of(0x1000) {
        return Err(MapError::NotAligned);
    }
    phys.checked_add(len).filter(|&e| e <= crate::VMAP_BASE - crate::DIRECT_MAP_BASE).ok_or(MapError::WrongRange)?;
    let root = super::kernel_root().ok_or(MapError::WrongRange)?;
    if cache != CacheType::WriteBack && crate::pat::read_pat() != crate::pat::LINUX_PAT {
        return Err(MapError::Unsupported);
    }
    let virt = crate::phys_to_virt(phys);
    let _irq = crate::cpu::IrqGuard::new();
    while BUSY.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        // The holder may be waiting for this CPU's answer to its shootdown.
        super::tlb::poll();
        core::hint::spin_loop();
    }
    let r = retype_locked(root, virt, len, cache);
    BUSY.store(false, Ordering::Release);
    r
}

/// The retype itself, with `BUSY` held and interrupts off.
fn retype_locked(root: u64, virt: u64, len: u64, cache: CacheType) -> Result<RetypeStats, MapError> {
    let mut st = RetypeStats::default();
    let split = super::kernel::with_frames(|d| split_range(d, root, virt, len, &mut st));
    // Also after a failed split: the upper entries it changed must not stay cached anywhere.
    if st.split_1g + st.split_2m > 0 && !super::tlb::flush_kernel_all().confirmed() {
        return Err(MapError::NeedsShootdown);
    }
    split??;
    let pages = super::kernel::with_frames(|d| set_cache_range(d, root, virt, len, cache.bits()));
    let confirmed = super::tlb::flush_kernel_all().confirmed();
    st.pages = pages??;
    if !confirmed {
        return Err(MapError::NeedsShootdown);
    }
    Ok(st)
}

#[cfg(test)]
#[path = "retype_tests.rs"]
mod tests;
