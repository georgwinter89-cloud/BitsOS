//! The one door to the chip (spec/wlan.md §4): register window (BAR2), PCI
//! config space, DMA memory, delays and time. The required methods are the op
//! kinds of V1's register scripts (kit/wlkit.mjs, io.inc), so a trace bus can
//! record every access exactly as the JS driver's fake does; the provided
//! methods are the shapes Linux rtw89 writes (rtw89_write32_set, _mask, ...).

/// Width of a register access
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    W8 = 1,
    W16 = 2,
    W32 = 4,
}

impl Width {
    pub const fn bytes(self) -> u8 {
        self as u8
    }
    /// The bits an access of this width can carry
    pub const fn mask(self) -> u32 {
        match self {
            Width::W8 => 0xff,
            Width::W16 => 0xffff,
            Width::W32 => 0xffff_ffff,
        }
    }
}

/// Width of a single traced write into DMA memory (kit's mw8, mw32, mw64)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemWidth {
    M8 = 1,
    M32 = 4,
    M64 = 8,
}

/// The longest wait any poll may have (V1 PORTING.md "Robustness" 1, `wh_clamp`)
pub const MAX_WAIT_MS: u16 = 10_000;

/// What went wrong at the bus. Values are kept so the error names register and value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// `(read & mask) != want` until the deadline; `last` is one extra read after it
    PollTimeout {
        addr: u32,
        width: Width,
        mask: u32,
        want: u32,
        last: u32,
    },
    /// An access outside the chip's windows (BAR2, its DMA, writable config dwords)
    Refused {
        kind: &'static str,
        addr: u32,
        len: u32,
    },
    /// No DMA memory below 4 GB for this request (`align` 0 or not a power of two is refused too)
    DmaExhausted { size: usize, align: usize },
    /// The chip or the fake chip no longer answers
    Gone,
    /// The host stream to the fake chip failed (trace bus only)
    Transport,
}

pub type BusResult<T> = Result<T, BusError>;

/// Chip access for the driver. All addresses of DMA memory are bus addresses
/// below 4 GB: the 8852BE behind the Mini-PC's AMD bridge cannot reach more
/// (spec/wlan.md §2.1 "DMA addressing").
pub trait Bus {
    /// A register read of BAR2 (op rd)
    fn read(&mut self, w: Width, addr: u32) -> BusResult<u32>;
    /// A register write of BAR2 (op wr); `val` already fits `w`
    fn write(&mut self, w: Width, addr: u32, val: u32) -> BusResult<()>;
    /// Read, clear `clear`, set `set`, write back (op rmw)
    fn rmw(&mut self, w: Width, addr: u32, clear: u32, set: u32) -> BusResult<()>;
    /// Until `(read & mask) == want`, at most `timeout_ms` (op poll). Implementations
    /// clamp the wait to [`MAX_WAIT_MS`]; see [`poll_loop`] for real hardware.
    fn poll(&mut self, w: Width, addr: u32, mask: u32, want: u32, timeout_ms: u16)
    -> BusResult<()>;
    /// Wait (op delay)
    fn delay_us(&mut self, us: u32) -> BusResult<()>;
    /// A dword of the function's config space (op cfgr)
    fn cfg_read(&mut self, off: u16) -> BusResult<u32>;
    /// A dword into config space (op cfgw); only the command register and DEVCTL2 are ever written
    fn cfg_write(&mut self, off: u16, val: u32) -> BusResult<()>;
    /// Coherent, uncached DMA memory below 4 GB, `align` a power of two; returns its bus address
    fn dma_alloc(&mut self, size: usize, align: usize) -> BusResult<u32>;
    /// Gives back what `dma_alloc` handed out
    fn dma_free(&mut self, addr: u32, size: usize) -> BusResult<()>;
    /// One traced write into DMA memory (op mwr; M64: the high half is `b`, the low `c`)
    fn mem_write(&mut self, w: MemWidth, addr: u32, val: u64) -> BusResult<()>;
    /// One traced dword read of DMA memory (op mrd)
    fn mem_read32(&mut self, addr: u32) -> BusResult<u32>;
    /// Bytes into DMA memory, not traced op by op (firmware pieces, frames)
    fn mem_write_bytes(&mut self, addr: u32, data: &[u8]) -> BusResult<()>;
    /// Bytes out of DMA memory, not traced op by op
    fn mem_read_bytes(&mut self, addr: u32, out: &mut [u8]) -> BusResult<()>;
    /// Milliseconds of a monotonic clock
    fn now_ms(&mut self) -> u64;

    /// A `# ` line in the trace (stage starts, blocks that are not Linux's); nothing on hardware
    fn note(&mut self, _text: &str) -> BusResult<()> {
        Ok(())
    }
    /// The JS driver's `await d.flush()`: on a batching bus everything sent has run
    fn flush(&mut self) -> BusResult<()> {
        Ok(())
    }

    // ---- the shapes of Linux rtw89 (rtw89_read32, rtw89_write32_set, ...) ----

    fn r8(&mut self, a: u32) -> BusResult<u8> {
        Ok(self.read(Width::W8, a)? as u8)
    }
    fn r16(&mut self, a: u32) -> BusResult<u16> {
        Ok(self.read(Width::W16, a)? as u16)
    }
    fn r32(&mut self, a: u32) -> BusResult<u32> {
        self.read(Width::W32, a)
    }
    /// rtw89_read32_mask: the field under `mask`, shifted down
    fn r32_mask(&mut self, a: u32, mask: u32) -> BusResult<u32> {
        Ok(field(self.r32(a)?, mask))
    }
    fn w8(&mut self, a: u32, v: u32) -> BusResult<()> {
        self.write(Width::W8, a, v & 0xff)
    }
    fn w16(&mut self, a: u32, v: u32) -> BusResult<()> {
        self.write(Width::W16, a, v & 0xffff)
    }
    fn w32(&mut self, a: u32, v: u32) -> BusResult<()> {
        self.write(Width::W32, a, v)
    }
    /// rtw89_write{8,16,32}_set
    fn set(&mut self, w: Width, a: u32, bits: u32) -> BusResult<()> {
        self.rmw(w, a, 0, bits & w.mask())
    }
    /// rtw89_write{8,16,32}_clr
    fn clr(&mut self, w: Width, a: u32, bits: u32) -> BusResult<()> {
        self.rmw(w, a, bits & w.mask(), 0)
    }
    /// rtw89_write{8,16,32}_mask: `v` is shifted to the mask's lowest bit
    fn mask(&mut self, w: Width, a: u32, mask: u32, v: u32) -> BusResult<()> {
        self.rmw(w, a, mask & w.mask(), prep(mask, v) & w.mask())
    }
    fn set32(&mut self, a: u32, bits: u32) -> BusResult<()> {
        self.set(Width::W32, a, bits)
    }
    fn clr32(&mut self, a: u32, bits: u32) -> BusResult<()> {
        self.clr(Width::W32, a, bits)
    }
    fn mask32(&mut self, a: u32, mask: u32, v: u32) -> BusResult<()> {
        self.mask(Width::W32, a, mask, v)
    }
    fn poll32(&mut self, a: u32, mask: u32, want: u32, ms: u16) -> BusResult<()> {
        self.poll(Width::W32, a, mask, want, ms)
    }
    fn mw32(&mut self, addr: u32, v: u32) -> BusResult<()> {
        self.mem_write(MemWidth::M32, addr, v as u64)
    }
}

/// The lowest set bit of `mask` (32 for 0, as kit's `lowbit`)
pub const fn lowbit(mask: u32) -> u32 {
    mask.trailing_zeros()
}

/// FIELD_GET: the bits under `mask`, shifted down
pub const fn field(v: u32, mask: u32) -> u32 {
    if mask == 0 {
        0
    } else {
        (v & mask) >> lowbit(mask)
    }
}

/// FIELD_PREP: `v` shifted to the mask's lowest bit, cut to the mask
pub const fn prep(mask: u32, v: u32) -> u32 {
    v.wrapping_shl(lowbit(mask)) & mask
}

/// read_poll_timeout for a bus on real hardware: reads every `step_us` until
/// the condition holds or the (clamped) deadline passed; then one more read,
/// whose value decides and goes into the error, as Linux does. The deadline is
/// also counted in waited steps (at least 1 us each), so a clock that does not
/// move (or moves only by whole ms) cannot make it endless.
pub fn poll_loop<B: Bus + ?Sized>(
    bus: &mut B,
    w: Width,
    addr: u32,
    mask: u32,
    want: u32,
    timeout_ms: u16,
    step_us: u32,
) -> BusResult<()> {
    let limit = u64::from(timeout_ms.min(MAX_WAIT_MS));
    let step_us = step_us.max(1);
    let start = bus.now_ms();
    let mut waited_us: u64 = 0;
    loop {
        if bus.read(w, addr)? & mask == want {
            return Ok(());
        }
        if bus.now_ms().saturating_sub(start) >= limit || waited_us >= limit * 1000 {
            let last = bus.read(w, addr)?;
            if last & mask == want {
                return Ok(());
            }
            return Err(BusError::PollTimeout {
                addr,
                width: w,
                mask,
                want,
                last,
            });
        }
        bus.delay_us(step_us)?;
        waited_us += u64::from(step_us);
    }
}
