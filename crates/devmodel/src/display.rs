//! `C: DisplayDevice` (V2-PLAN §3, spec/gpu.md §5.2; back ends gopfb (DP), amdgpu::dcn (GX)). Fences are
//! syncobj-compatible from the start (GX-D4), display objects follow KMS concepts (GX-D5).

use crate::DevError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit, blue in the lowest byte.
    Bgrx8888,
    Rgbx8888,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Millihertz (59950 = 59.95 Hz); 0 = unknown (GOP).
    pub refresh_mhz: u32,
}

/// A buffer the display can scan out (GX-D3: a buffer object later).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanoutId(pub u64);

/// Completion of a flip; `signalled` once the new buffer is on screen (GOP: at once).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fence {
    pub seq: u64,
    pub signalled: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct VblankEvent {
    pub sequence: u64,
    pub time_ns: u64,
}

pub trait DisplayDevice: Send + Sync {
    fn name(&self) -> &str;
    fn modes(&self) -> alloc::vec::Vec<Mode>;
    fn current_mode(&self) -> Mode;
    fn format(&self) -> PixelFormat;
    /// Copies the damaged rectangle `(x, y, w, h)` of a full frame `src` (stride in pixels) to the screen.
    /// GOP back end: the only way a picture reaches the scanout (it never reads the framebuffer).
    fn present(&self, src: &[u32], stride: u32, damage: (u32, u32, u32, u32)) -> Result<Fence, DevError>;
    /// Next vblank (GOP: a timer estimate; DCN: OTG frame counter, later IH).
    fn last_vblank(&self) -> VblankEvent;
    /// OTG CRC of the scanout (DCN) for tests: "the monitor really got this picture".
    fn read_crc(&self) -> Option<u32> {
        None
    }
}
