//! Unsafe primitives of the DP driver lane. Owner: DP.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
//!
//! What lives here: `FbWindow`, the GOP framebuffer mapped write-combining through MM-2's vmap
//! (`paging::map_device_window`), and the two things gopfb does with it: bounded copies into it (`rep movsb`;
//! ERMS/FSRM on both machines, CONTRACTS §6a: no SSE in the kernel) and an `sfence` that drains the
//! write-combining buffers. Reading the framebuffer is 25-100x slower than writing it (spec/gpu.md §2.3); the
//! picture's truth is the RAM frame. The one read (WY-17: MCP `screen` of a frame the GPU drew straight into a
//! scanout buffer) uses streaming loads in a guarded SIMD section (SHOT, `stream16`).
//!
//! Soundness: `map_device_window` refuses RAM and verifies the kernel page tables (every page present,
//! writable, NX, not user, exactly the asked physical range with the asked type) before it hands out a
//! window; the `DeviceWindow` is owned by the `FbWindow`, so the mapping lives as long as any copy can name
//! it. Device memory is no Rust object, so a store there aliases nothing. Every copy is bounds-checked against
//! the window length with checked arithmetic, and the source is a Rust slice (valid for its length; a `u32`
//! slice is read as its bytes, which is fine: u32 has no padding).
//!
//! Host builds (unit tests): no window can be mapped (no device mapper), and the copy refuses anyway.
//!
//! DP-2: `copy_from_region_u32` reads fbdev's RAM pages (MM-4 DMA buffers that programs map) straight into a
//! RAM frame's `u32` pixels with one `rep movsb` (the safe `DmaRegion::copy_out` only fills byte slices, which
//! would cost a second copy per frame).

use crate::dma_buf::DmaRegion;
use crate::paging::{self, CacheType, DeviceWindow, VmapError};

/// Copies `dst.len()` 32-bit pixels from byte `off` of the DMA region `r` into `dst`; false (nothing copied)
/// if they do not lie inside the region. Programs may store into the region at the same time (their mapping
/// of fbdev's pages): the copy then holds old or new bytes of those pixels, never anything outside.
pub fn copy_from_region_u32(r: &DmaRegion, off: usize, dst: &mut [u32]) -> bool {
    let Some(n) = dst.len().checked_mul(4) else { return false };
    if off.checked_add(n).is_none_or(|e| e > r.len()) {
        return false;
    }
    let Some(src) = r.cpu_addr().checked_add(off as u64) else { return false };
    if n == 0 {
        return true;
    }
    let dst = dst.as_mut_ptr().cast::<u8>();
    // SAFETY: `DmaRegion::new`'s contract (only mm-core's DMA allocation makes one) says [cpu_addr, cpu_addr +
    // len) is mapped, readable memory the region owns for its whole life, and `r` is borrowed for this call;
    // [src, src + n) lies inside it (checked above, no overflow). That memory is no Rust object (devices and
    // programs write it), so reading it aliases nothing. `dst` is a live, exclusively borrowed slice of `n`
    // bytes (u32 has no padding and every byte pattern is a valid u32), so the two never overlap. DF is clear
    // (Rust ABI); `rep movsb` moves exactly rcx bytes and changes no flags.
    unsafe {
        core::arch::asm!(
            "rep movsb",
            inout("rcx") n => _,
            inout("rsi") src => _,
            inout("rdi") dst => _,
            options(nostack, preserves_flags)
        );
    }
    true
}

/// A write-only, write-combining window over a framebuffer.
pub struct FbWindow {
    win: DeviceWindow,
}

impl FbWindow {
    /// Maps `len` bytes of the framebuffer at `phys` write-combining through the vmap allocator. May wait for
    /// the vmap mutex (not in interrupt context, not in a B4 no-wait section).
    pub fn map_wc(phys: u64, len: u64) -> Result<FbWindow, VmapError> {
        let win = paging::map_device_window(phys, len, CacheType::WriteCombining)?;
        if win.cache() != CacheType::WriteCombining || win.len() != len || win.phys() != phys {
            return Err(VmapError::Verify);
        }
        Ok(FbWindow { win })
    }

    pub fn len(&self) -> usize {
        self.win.len() as usize
    }
    pub fn is_empty(&self) -> bool {
        self.win.is_empty()
    }
    pub fn phys(&self) -> u64 {
        self.win.phys()
    }
    /// Virtual address of byte 0 (for tests that check the page tables; never dereferenced outside this file).
    pub fn virt(&self) -> u64 {
        self.win.virt()
    }

    /// Copies `src` to byte `off` of the window; false (nothing written) if it does not fit.
    pub fn write_bytes(&self, off: usize, src: &[u8]) -> bool {
        self.copy_in(off, src.as_ptr(), src.len())
    }

    /// Copies 32-bit pixels (little-endian, as they lie in memory) to byte `off` of the window.
    pub fn write_pixels(&self, off: usize, src: &[u32]) -> bool {
        let Some(n) = src.len().checked_mul(4) else { return false };
        self.copy_in(off, src.as_ptr().cast::<u8>(), n)
    }

    /// WY-17: copies 32-bit pixels from byte `off` of the window into `dst`; false (nothing copied) if they do not
    /// lie inside. Reads write-combined memory, only on request (MCP `screen`): SHOT reads it with streaming loads
    /// (`stream_read`), the plain copy only for a window off the 16-byte grid (never seen: windows start on a page).
    pub fn read_pixels(&self, off: usize, dst: &mut [u32]) -> bool {
        let Some(n) = dst.len().checked_mul(4) else { return false };
        if !crate::BARE_METAL || off.checked_add(n).is_none_or(|e| e > self.len()) {
            return false;
        }
        if n == 0 {
            return true;
        }
        let base = self.win.virt() as usize;
        // SAFETY: `dst` is a live, exclusively borrowed slice of `n` bytes (u32 has no padding and every byte
        // pattern is a valid u32), viewed as bytes for the copy only.
        let bytes = unsafe { core::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), n) };
        if base.is_multiple_of(16) && self.len().is_multiple_of(16) {
            // SAFETY: [base, base + len) is the mapped, verified device window this FbWindow keeps mapped, readable,
            // 16-byte aligned and a multiple of 16 long (checked); off + n <= len (checked above, no overflow).
            unsafe { stream_read(base, self.len(), off, bytes) };
            return true;
        }
        let src = base.wrapping_add(off);
        // SAFETY: the source `[src, src + n)` lies inside the mapped, verified device window (checked above, no
        // overflow), which this FbWindow keeps mapped; device memory is no Rust object, so nothing is aliased. `bytes`
        // is RAM, so the two never overlap. DF is clear (Rust ABI).
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") n => _,
                inout("rsi") src => _,
                inout("rdi") bytes.as_mut_ptr() => _,
                options(nostack, preserves_flags)
            );
        }
        true
    }

    /// `n` bytes from `src` (the pointer and byte length of a live slice) to byte `off`.
    fn copy_in(&self, off: usize, src: *const u8, n: usize) -> bool {
        if !crate::BARE_METAL || off.checked_add(n).is_none_or(|e| e > self.len()) {
            return false;
        }
        if n == 0 {
            return true;
        }
        let dst = (self.win.virt() as usize).wrapping_add(off);
        // SAFETY: the destination `[dst, dst + n)` lies inside the mapped, verified device window (checked
        // above, no overflow), which this FbWindow keeps mapped; the source is `n` readable bytes of a live
        // slice (both callers pass a slice's pointer and byte length). DF is clear (Rust ABI). Device memory is
        // no Rust object, so nothing is aliased.
        unsafe {
            core::arch::asm!(
                "rep movsb",
                inout("rcx") n => _,
                inout("rsi") src => _,
                inout("rdi") dst => _,
                options(nostack, preserves_flags)
            );
        }
        true
    }

    /// Drains the write-combining buffers (after a present, before a fence is signalled).
    pub fn flush(&self) {
        if crate::BARE_METAL {
            // SAFETY: `sfence` only orders stores; it has no operands.
            unsafe { core::arch::asm!("sfence", options(nostack, preserves_flags)) };
        }
    }
}

/// SHOT: `dst.len()` bytes from byte `off` of the readable range [base, base + len) into `dst`, with streaming loads
/// (`stream16`) on the 16-byte blocks that hold them: a block only partly wanted goes through a small buffer on the
/// stack, the rest straight into `dst`.
///
/// # Safety
/// [base, base + len) is mapped and readable for the whole call and is no Rust object `dst` could alias; `base` is
/// 16-byte aligned, `len` a multiple of 16, and `off + dst.len() <= len`.
unsafe fn stream_read(base: usize, len: usize, off: usize, dst: &mut [u8]) {
    let end = off + dst.len();
    debug_assert!(base.is_multiple_of(16) && len.is_multiple_of(16) && end <= len);
    let mut pos = off;
    while pos < end {
        let block = pos & !15;
        if pos == block && end - pos >= 16 {
            let run = (end - pos) & !15;
            // SAFETY: [base + pos, base + pos + run) lies in the range (pos + run <= end <= len), 16-byte aligned
            // with `run` a multiple of 16; `dst[pos - off..]` holds at least `run` bytes (pos + run <= end).
            unsafe { stream16(base + pos, dst[pos - off..].as_mut_ptr(), run) };
            pos += run;
        } else {
            let mut tmp = [0u8; 16];
            // SAFETY: the block [base + block, base + block + 16) lies in the range: block <= pos < end <= len and len
            // is a multiple of 16, so block + 16 <= len; `tmp` holds 16 bytes.
            unsafe { stream16(base + block, tmp.as_mut_ptr(), 16) };
            let take = (block + 16).min(end) - pos;
            dst[pos - off..pos - off + take].copy_from_slice(&tmp[pos - block..pos - block + take]);
            pos += take;
        }
    }
}

/// SHOT: the kernel's one guarded SIMD section (R10, CONTRACTS §1: vector code only inside hal): `n` bytes from `src`
/// to `dst` with SSE4.1 MOVNTDQA, which on write-combined memory fetches each 64-byte line once into a streaming
/// buffer instead of one uncached bus read per load (Intel's SSE4.1 streaming-load guide; Linux
/// `drm_memcpy_from_wc`, linux-7.3.1/drivers/gpu/drm/drm_cache.c). x86-64-v3 (R10) includes SSE4.1.
///
/// The guard: the vector registers belong to the thread that runs (hal/xsave.rs), so the block saves xmm0-xmm3 into a
/// stack buffer first and restores them last, in the same asm block; the compiler never sees them (soft-float). The
/// legacy (non-VEX) encodings leave the upper ymm/zmm bits alone; integer moves leave MXCSR alone. An interrupt in
/// between touches no vector register (soft-float handlers) and a switch saves and restores the whole state, the
/// borrowed registers included. The fences: the first keeps a stale streaming buffer from an earlier read out, the
/// last orders these weakly ordered loads before the caller's next load (the check that the buffer was still held).
///
/// # Safety
/// [src, src + n) is mapped, readable and no Rust object `dst` could alias; `src` 16-byte aligned; `n` a multiple
/// of 16; `dst` valid for `n` bytes of writes.
unsafe fn stream16(src: usize, dst: *mut u8, n: usize) {
    // Not zeroed: on a build with SSE code generation (host tests) the zeroing itself would use xmm0.
    let mut save = core::mem::MaybeUninit::<[u8; 64]>::uninit();
    // SAFETY: the caller's contract covers the loads from [src, src + n) and the stores to [dst, dst + n); `save` is
    // a live 64-byte local, written before it is read. xmm0-xmm3 are restored before the block ends, so no register the compiler knows changes
    // but rsi, rdi, rcx (declared) and the flags (not preserved).
    unsafe {
        core::arch::asm!(
            "movdqu xmmword ptr [{save}], xmm0",
            "movdqu xmmword ptr [{save} + 16], xmm1",
            "movdqu xmmword ptr [{save} + 32], xmm2",
            "movdqu xmmword ptr [{save} + 48], xmm3",
            "mfence",
            "cmp rcx, 64",
            "jb 3f",
            "2:",
            "movntdqa xmm0, xmmword ptr [rsi]",
            "movntdqa xmm1, xmmword ptr [rsi + 16]",
            "movntdqa xmm2, xmmword ptr [rsi + 32]",
            "movntdqa xmm3, xmmword ptr [rsi + 48]",
            "movdqu xmmword ptr [rdi], xmm0",
            "movdqu xmmword ptr [rdi + 16], xmm1",
            "movdqu xmmword ptr [rdi + 32], xmm2",
            "movdqu xmmword ptr [rdi + 48], xmm3",
            "add rsi, 64",
            "add rdi, 64",
            "sub rcx, 64",
            "cmp rcx, 64",
            "jae 2b",
            "3:",
            "test rcx, rcx",
            "jz 5f",
            "4:",
            "movntdqa xmm0, xmmword ptr [rsi]",
            "movdqu xmmword ptr [rdi], xmm0",
            "add rsi, 16",
            "add rdi, 16",
            "sub rcx, 16",
            "jnz 4b",
            "5:",
            "mfence",
            "movdqu xmm0, xmmword ptr [{save}]",
            "movdqu xmm1, xmmword ptr [{save} + 16]",
            "movdqu xmm2, xmmword ptr [{save} + 32]",
            "movdqu xmm3, xmmword ptr [{save} + 48]",
            save = in(reg) save.as_mut_ptr().cast::<u8>(),
            inout("rcx") n => _,
            inout("rsi") src => _,
            inout("rdi") dst => _,
            options(nostack)
        );
    }
}

impl core::fmt::Debug for FbWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "FbWindow({:?})", self.win)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn no_window_on_the_host() {
        assert!(FbWindow::map_wc(0xC000_0000, 4096).is_err());
    }

    /// 16-byte aligned RAM standing in for a window (the streaming loads read RAM like any load).
    #[repr(align(64))]
    struct Window([u8; 512]);

    /// Every start and length against a plain copy: the partly wanted blocks at both ends, a run inside one block,
    /// runs of whole lines and of single blocks.
    #[test]
    fn streaming_reads_match_the_bytes_at_every_offset() {
        let mut w = Window([0; 512]);
        for (i, b) in w.0.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37) ^ (i >> 8) as u8;
        }
        let base = w.0.as_ptr() as usize;
        for off in 0..80 {
            for n in [0, 1, 3, 4, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 200, 431 - off] {
                let mut dst = vec![0xEEu8; n + 1];
                // SAFETY: `w` is 512 readable bytes, 64-byte aligned, and off + n <= 512.
                unsafe { stream_read(base, 512, off, &mut dst[..n]) };
                assert_eq!(&dst[..n], &w.0[off..off + n], "off {off} n {n}");
                assert_eq!(dst[n], 0xEE, "off {off} n {n}: a byte past the end was written");
            }
        }
    }

    /// `stream16` behind the C ABI, so the test below can call it from one asm block (the compiler may use vector
    /// registers anywhere in host code, but not inside an asm block).
    #[inline(never)]
    extern "sysv64" fn stream16_c(src: usize, dst: *mut u8, n: usize) {
        // SAFETY: the one caller passes a readable, aligned source and a writable destination of `n` bytes.
        unsafe { stream16(src, dst, n) }
    }

    /// The borrowed registers come back: what the thread had in xmm0-xmm3 before the copy, it has after it.
    #[test]
    fn the_vector_registers_survive_a_streaming_read() {
        let w = Window([0x5A; 512]);
        let before: [u8; 64] = core::array::from_fn(|i| i as u8 ^ 0xC3);
        let mut after = [0u8; 64];
        let mut dst = [0u8; 200];
        // SAFETY: the block reads `before` and writes `after` (64 bytes each); the call copies 192 bytes of `w` (512
        // readable, 64-byte aligned bytes) into `dst` (200 bytes) and follows the C ABI, whose clobbers are declared;
        // the stack is realigned to 16 for the call and put back from r12 (declared).
        unsafe {
            core::arch::asm!(
                "movdqu xmm0, xmmword ptr [{b}]",
                "movdqu xmm1, xmmword ptr [{b} + 16]",
                "movdqu xmm2, xmmword ptr [{b} + 32]",
                "movdqu xmm3, xmmword ptr [{b} + 48]",
                "mov r12, rsp",
                "and rsp, -16",
                "call {f}",
                "mov rsp, r12",
                "movdqu xmmword ptr [r13], xmm0",
                "movdqu xmmword ptr [r13 + 16], xmm1",
                "movdqu xmmword ptr [r13 + 32], xmm2",
                "movdqu xmmword ptr [r13 + 48], xmm3",
                b = in(reg) before.as_ptr(),
                in("r13") after.as_mut_ptr(),
                f = sym stream16_c,
                in("rdi") w.0.as_ptr() as usize,
                in("rsi") dst.as_mut_ptr(),
                in("rdx") 192usize,
                out("r12") _,
                clobber_abi("sysv64"),
            );
        }
        assert_eq!(after, before);
        assert!(dst[..192].iter().all(|&b| b == 0x5A) && dst[192..].iter().all(|&b| b == 0));
    }
}
