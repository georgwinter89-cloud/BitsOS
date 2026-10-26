//! Where gopfb's copies land: the WC framebuffer window from MM-2's vmap (normal case), bitsboot's early
//! mapping (fallback when the vmap refuses), or plain RAM (tests; DP-2's deferred io may reuse it). Write-only
//! interface: nothing here reads a framebuffer back.

use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::drv_dp::FbWindow;
use hal::mmio::IoMem;
use ksync::SpinLock;

pub trait FbTarget: Send + Sync {
    /// Bytes the target holds.
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// 32-bit pixels, little-endian, to byte `off`; false (nothing written) if they do not fit.
    fn write_pixels(&self, off: usize, px: &[u32]) -> bool;
    /// Bytes to byte `off` (layouts with 2 or 3 bytes per pixel).
    fn write_bytes(&self, off: usize, b: &[u8]) -> bool;
    /// Makes the writes so far reach the device (WC: `sfence`).
    fn flush(&self) {}
    /// (virtual, physical) address of byte 0 when this is a mapped framebuffer window (for the WC ktest).
    fn window(&self) -> Option<(u64, u64)> {
        None
    }
}

impl FbTarget for FbWindow {
    fn len(&self) -> usize {
        FbWindow::len(self)
    }
    fn write_pixels(&self, off: usize, px: &[u32]) -> bool {
        FbWindow::write_pixels(self, off, px)
    }
    fn write_bytes(&self, off: usize, b: &[u8]) -> bool {
        FbWindow::write_bytes(self, off, b)
    }
    fn flush(&self) {
        FbWindow::flush(self)
    }
    fn window(&self) -> Option<(u64, u64)> {
        Some((self.virt(), self.phys()))
    }
}

/// bitsboot's mapping of the framebuffer (WC once the PAT is programmed), used only when the vmap refuses.
pub struct BootWindow(pub IoMem);

impl FbTarget for BootWindow {
    fn len(&self) -> usize {
        self.0.size()
    }
    fn write_pixels(&self, off: usize, px: &[u32]) -> bool {
        let Some(end) = px.len().checked_mul(4).and_then(|n| off.checked_add(n)) else { return false };
        if end > self.0.size() {
            return false;
        }
        let mut buf = [0u8; 256];
        let mut o = off;
        for chunk in px.chunks(64) {
            for (i, p) in chunk.iter().enumerate() {
                buf[i * 4..i * 4 + 4].copy_from_slice(&p.to_le_bytes());
            }
            let n = chunk.len() * 4;
            if self.0.write_bytes(o, &buf[..n]).is_err() {
                return false;
            }
            o += n;
        }
        true
    }
    fn write_bytes(&self, off: usize, b: &[u8]) -> bool {
        self.0.write_bytes(off, b).is_ok()
    }
}

/// A framebuffer in RAM with a byte counter: the test double for the damage-copy checks.
pub struct RamTarget {
    bytes: SpinLock<Vec<u8>>,
    written: AtomicU64,
}

impl RamTarget {
    /// `len` bytes, each set to `fill` (a sentinel the checks look for).
    pub fn new(len: usize, fill: u8) -> RamTarget {
        // `display.` prefix: the lock may be taken under display.frame (ksync/order.rs).
        RamTarget { bytes: SpinLock::named("display.ram_target", vec![fill; len]), written: AtomicU64::new(0) }
    }
    /// Bytes written since creation.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }
    pub fn read(&self, off: usize, out: &mut [u8]) -> bool {
        let g = self.bytes.lock();
        match off.checked_add(out.len()).and_then(|e| g.get(off..e)) {
            Some(s) => {
                out.copy_from_slice(s);
                true
            }
            None => false,
        }
    }
    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes.lock().clone()
    }
}

impl FbTarget for RamTarget {
    fn len(&self) -> usize {
        self.bytes.lock().len()
    }
    fn write_pixels(&self, off: usize, px: &[u32]) -> bool {
        let mut g = self.bytes.lock();
        let Some(dst) = px.len().checked_mul(4).and_then(|n| off.checked_add(n)).and_then(|e| g.get_mut(off..e)) else {
            return false;
        };
        for (d, p) in dst.as_chunks_mut::<4>().0.iter_mut().zip(px) {
            d.copy_from_slice(&p.to_le_bytes());
        }
        self.written.fetch_add(px.len() as u64 * 4, Ordering::Relaxed);
        true
    }
    fn write_bytes(&self, off: usize, b: &[u8]) -> bool {
        let mut g = self.bytes.lock();
        let Some(dst) = off.checked_add(b.len()).and_then(|e| g.get_mut(off..e)) else { return false };
        dst.copy_from_slice(b);
        self.written.fetch_add(b.len() as u64, Ordering::Relaxed);
        true
    }
}
