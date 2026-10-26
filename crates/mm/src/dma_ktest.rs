//! ktests of the DMA API (MM-4, contract `C: DmaPool`): address masks, boundaries never crossed, alignment,
//! exact contiguous runs, UC/WC through the direct map's PAT bits, free and reuse, hostile requests refused,
//! and the typed accessors.

use crate::dma::{DmaBuffer, DmaCache, DmaConstraints, DmaError};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use hal::paging::{self, CacheType, PageSize};
use ktest::{kassert, ktest, TestResult};
use mm_core::{boot_buddy, Kind, PAGE};

const GIB: u64 = 1 << 30;
const KIB: usize = 1024;

fn get(len: usize, c: DmaConstraints) -> Result<DmaBuffer, String> {
    DmaBuffer::alloc(len, c).map_err(|e| format!("{len} bytes {c:?}: {e:?}"))
}

fn dma_used() -> u64 {
    mm_core::frames().map(|f| f.stats().used_by_kind[Kind::Dma as usize]).unwrap_or(0)
}

fn masks() -> TestResult {
    let b = get(64 * KIB, DmaConstraints::dma32())?;
    kassert!(b.device_addr() + (64 << 10) <= 4 * GIB, "DMA32 buffer at {:#x}", b.device_addr());
    for bits in [30u32, 28, 24] {
        let c = DmaConstraints::from_mask_bits(bits);
        match DmaBuffer::alloc(3 * PAGE as usize, c) {
            Ok(x) => kassert!(x.device_addr() + 3 * PAGE <= 1 << bits, "{bits}-bit buffer at {:#x}", x.device_addr()),
            Err(DmaError::NoMemory) if bits <= 24 => {}
            Err(e) => return Err(format!("{bits}-bit mask: {e:?}")),
        }
    }
    let any = get(16 * KIB, DmaConstraints::dma64())?;
    let normal = boot_buddy().map(|b| b.zone_info(1)).ok_or("no buddy")?;
    let above = any.device_addr() >= 4 * GIB;
    klog::event!(Info, "mm", "ktest.dma_mask", dma32 = klog::Hex(b.device_addr()), dma64 = klog::Hex(any.device_addr()), dma64_above_4g = above,
        normal_free_mb = ((normal.free_frames + normal.cached_frames) * PAGE) >> 20);
    if normal.free_frames + normal.cached_frames > 64 {
        kassert!(above, "64-bit buffer at {:#x}: NORMAL comes first", any.device_addr());
    }
    Ok(())
}
ktest!("mm.dma.mask", Safe, masks);

fn boundary() -> TestResult {
    // Many buffers at once, so they land at many different places.
    for (bound, sizes) in [(64u64 << 10, [4 * KIB, 12 * KIB, 40 * KIB, 64 * KIB, 60 * KIB + 1]), (16 << 10, [4 * KIB, 12 * KIB, 16 * KIB, 8 * KIB + 3, 100])] {
        let mut held = Vec::new();
        for i in 0..150 {
            let len = sizes[i % sizes.len()];
            let b = get(len, DmaConstraints::dma64().with_boundary(bound))?;
            let (s, e) = (b.device_addr(), b.device_addr() + len as u64 - 1);
            kassert!(s / bound == e / bound, "{len} bytes at {s:#x} cross a {bound:#x} boundary");
            held.push(b);
        }
    }
    kassert!(DmaBuffer::alloc(64 * KIB + 1, DmaConstraints::dma32().with_boundary(64 << 10)).err() == Some(DmaError::Invalid));
    Ok(())
}
ktest!("mm.dma.boundary", Safe, boundary);

fn alignment() -> TestResult {
    for align in [8u64 << 10, 64 << 10, 256 << 10, 2 << 20] {
        for len in [100usize, 4 * KIB, 20 * KIB] {
            let b = get(len, DmaConstraints::dma64().with_align(align))?;
            kassert!(b.device_addr() % align == 0, "{len} bytes aligned {align:#x} at {:#x}", b.device_addr());
        }
    }
    kassert!(DmaBuffer::alloc(4096, DmaConstraints::dma64().with_align(3 << 12)).err() == Some(DmaError::Invalid));
    Ok(())
}
ktest!("mm.dma.align", Safe, alignment);

fn contiguous() -> TestResult {
    let buddy = boot_buddy().ok_or("no buddy")?;
    for len in [5 * PAGE as usize, 300 * KIB, 1 << 20, 3 * PAGE as usize + 1] {
        let before = dma_used();
        let b = get(len, DmaConstraints::dma64())?;
        let pages = (len as u64).div_ceil(PAGE);
        kassert!(dma_used() - before == pages * PAGE, "{len} bytes charged {} (exact run, no rounding)", dma_used() - before);
        kassert!(buddy.run_allocated(b.device_addr(), pages, Kind::Dma), "not one exact run at {:#x}", b.device_addr());
        kassert!(b.to_vec().iter().all(|&x| x == 0), "not zeroed");
        kassert!(b.write_u64(len / 8 * 8 - 8, 0x1122_3344_5566_7788) && b.read_u64(len / 8 * 8 - 8) == Some(0x1122_3344_5566_7788));
        drop(b);
        kassert!(dma_used() == before, "frames not given back");
    }
    Ok(())
}
ktest!("mm.dma.contiguous", Safe, contiguous);

fn uc_wc_pat() -> TestResult {
    let root = paging::kernel_root().ok_or("kernel still on the loader's tables")?;
    // MM-6: with several CPUs the retype is a shootdown on all of them; the same checks hold.
    // dma64 lands above 4 GiB with 6G, where the direct map uses 1 GiB pages: the retype splits them.
    let kinds = [
        (DmaCache::Uncached, CacheType::Uncached, DmaConstraints::dma32()),
        (DmaCache::WriteCombining, CacheType::WriteCombining, DmaConstraints::dma32()),
        (DmaCache::WriteCombining, CacheType::WriteCombining, DmaConstraints::dma64()),
        (DmaCache::Uncached, CacheType::Uncached, DmaConstraints::dma64()),
    ];
    for (cache, want, c) in kinds {
        let len = 3 * PAGE as usize + 100;
        let b = get(len, c.with_cache(cache))?;
        let pages = (len as u64).div_ceil(PAGE);
        let (va, pa) = (b.cpu_addr(), b.phys());
        kassert!(va == hal::phys_to_virt(pa), "the CPU view is the retyped direct-map alias");
        for p in 0..pages {
            let l = paging::translate_leaf(root, va + p * PAGE).ok_or("buffer page unmapped")?;
            kassert!(l.size == PageSize::Size4K && l.flags.cache() == Some(want), "page {p}: {:?} {:?}", l.size, l.flags.cache());
            kassert!(l.phys == pa + p * PAGE);
        }
        for outside in [va.wrapping_sub(PAGE), va + pages * PAGE] {
            if let Some(l) = paging::translate_leaf(root, outside) {
                kassert!(l.flags.cache() == Some(CacheType::WriteBack), "neighbour {outside:#x} retyped");
            }
        }
        // The CPU view works through the new type.
        kassert!(b.to_vec().iter().all(|&x| x == 0), "{cache:?} buffer not zeroed");
        kassert!(b.write_u32(8, 0xC0FF_EE00) && b.copy_in(len - 3, &[7, 8, 9]));
        b.write_barrier();
        let mut t = [0u8; 3];
        kassert!(b.read_u32(8) == Some(0xC0FF_EE00) && b.copy_out(len - 3, &mut t) && t == [7, 8, 9]);
        drop(b);
        for p in 0..pages {
            let l = paging::translate_leaf(root, va + p * PAGE).ok_or("page unmapped after free")?;
            kassert!(l.flags.cache() == Some(CacheType::WriteBack), "page {p} not back to WB after free");
        }
    }
    kassert!(mm_core::dma::KEPT_UNTYPED.load(core::sync::atomic::Ordering::Relaxed) == 0);
    Ok(())
}
ktest!("mm.dma.uc_wc_pat", Safe, uc_wc_pat);

fn free_reuse() -> TestResult {
    let buddy = boot_buddy().ok_or("no buddy")?;
    let before = dma_used();
    // A power-of-two run is one buddy block: freed, it merges back and the next request gets it again. A
    // 3-page run leaves single frames in the per-CPU lists; after a drain they merge too.
    for (len, drain) in [(16 * KIB, false), (12 * KIB, true)] {
        let mut seen = Vec::new();
        for i in 0..60usize {
            let c = if i % 2 == 0 { DmaConstraints::dma32() } else { DmaConstraints::dma64().with_boundary(64 << 10) };
            let b = get(len, c)?;
            if i % 2 == 1 && !seen.contains(&b.device_addr()) {
                seen.push(b.device_addr());
            }
            drop(b);
            if drain {
                buddy.drain_all();
            }
        }
        kassert!(seen.len() <= 2, "{len} bytes: freed frames not reused ({} different places)", seen.len());
    }
    kassert!(dma_used() == before, "{} bytes of DMA frames not given back", dma_used() - before);
    kassert!(mm_core::dma::FREE_REFUSED.load(core::sync::atomic::Ordering::Relaxed) == 0, "the buddy refused a DMA free");
    Ok(())
}
ktest!("mm.dma.free_reuse", Safe, free_reuse);

fn hostile() -> TestResult {
    // SC-3: drivers on other CPUs allocate and free DMA memory meanwhile, so one comparison can differ by their
    // buffers; a refused request that keeps frames differs in every round.
    let mut last = String::new();
    for _ in 0..5 {
        match hostile_round() {
            Ok(()) => return Ok(()),
            Err(e) if e.contains("kept frames") => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn hostile_round() -> TestResult {
    let before = dma_used();
    let c = DmaConstraints::dma64();
    let cases: [(usize, DmaConstraints, DmaError); 9] = [
        (0, c, DmaError::Invalid),
        (usize::MAX, c, DmaError::TooLarge),
        ((1 << 30) + 1, c, DmaError::TooLarge),
        (4096, c.with_align(3 << 12), DmaError::Invalid),
        (4096, c.with_align(1 << 63), DmaError::TooLarge),
        (4096, c.with_boundary(5000), DmaError::Invalid),
        (8192, c.with_boundary(4096), DmaError::Invalid),
        (4096, DmaConstraints { limit: 0, ..c }, DmaError::Invalid),
        (4096, DmaConstraints::from_mask_bits(20), DmaError::NoMemory),
    ];
    for (len, c, want) in cases {
        let got = DmaBuffer::alloc(len, c).err();
        kassert!(got == Some(want), "{len} bytes {c:?}: {got:?}, wanted {want:?}");
    }
    // A full gigabyte run usually exists in QEMU's 2G; either way it must not panic or leak.
    let _ = DmaBuffer::alloc(1 << 30, c);
    kassert!(dma_used() == before, "a refused request kept frames ({} -> {} bytes)", before, dma_used());
    Ok(())
}
ktest!("mm.dma.hostile_sizes", Safe, hostile);

fn accessors() -> TestResult {
    let b = get(100, DmaConstraints::dma64())?;
    kassert!(b.len() == 100 && b.cpu().len() == 100);
    kassert!(b.write_u16(98, 0xABCD) && b.read_u16(98) == Some(0xABCD));
    kassert!(!b.write_u16(99, 1) && b.read_u16(99).is_none(), "misaligned u16");
    kassert!(!b.write_u32(98, 1), "u32 past the end");
    kassert!(b.write_u64(88, u64::MAX) && b.read_u32(92) == Some(u32::MAX));
    kassert!(b.read_u64(96).is_none() && b.read_u8(usize::MAX).is_none());
    kassert!(!b.write(99, &[1, 2]) && b.write(98, &[1, 2]) && b.read_u8(99) == Some(2));
    kassert!(b.fill(0, 100, 0x5A) && b.read_u64(0) == Some(0x5A5A_5A5A_5A5A_5A5A) && !b.fill(1, 100, 0));
    kassert!(b.zero() && b.to_vec().iter().all(|&x| x == 0));
    Ok(())
}
ktest!("mm.dma.accessors", Safe, accessors);
