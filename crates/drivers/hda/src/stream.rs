//! Stream descriptors: ring layout and buffer descriptor list, reset, programming, run/stop, and the play
//! position from LPIB with the AMD wrap quirk. After `linux-6.6/sound/hda/hdac_stream.c` (`stream_reset` :224,
//! `stream_setup` :256, `setup_periods` :488) and V1 `snd/stream.asm` (`sd_setup` :35), `snd/ring.asm:24-33`.

use crate::hw::{until, Hw, Mem};
use crate::regs::*;

/// The largest ring one stream may have, and all streams of a controller together: Linux's managed PCM buffer
/// limit (`linux-6.6/sound/pci/hda/hda_controller.c:693` MAX_PREALLOC_SIZE) and per-card total
/// (`sound/core/pcm_memory.c:30` max_alloc_per_card). SN-4 announces it as buffer_bytes_max.
pub const MAX_RING_BYTES: u32 = 32 * 1024 * 1024;

/// Ring and periods in bytes; `periods` BDL entries of `period_bytes` each (one per period: our rings are one
/// contiguous DMA buffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub frame_bytes: u32,
    pub ring_bytes: u32,
    pub period_bytes: u32,
    pub periods: usize,
}

impl Layout {
    /// A ring of at least `ring_frames` in periods of about `period_frames`: periods are multiples of 128 bytes
    /// (HDA buffers start on 128 bytes), 2 to 256 of them (LVI's 8 bits; more frames -> longer periods).
    pub fn new(frame_bytes: usize, ring_frames: u32, period_frames: u32) -> Option<Layout> {
        if frame_bytes == 0 || !BDL_ALIGN.is_multiple_of(frame_bytes) || ring_frames == 0 {
            return None;
        }
        let fb = frame_bytes as u64;
        let ring = ring_frames as u64 * fb;
        let mut period = (period_frames.max(1) as u64 * fb).min(ring / BDL_MIN as u64).max(BDL_ALIGN as u64);
        period = period.div_ceil(BDL_ALIGN as u64) * BDL_ALIGN as u64;
        let mut periods = ring.div_ceil(period).max(BDL_MIN as u64);
        if periods > BDL_MAX as u64 {
            period = ring.div_ceil(BDL_MAX as u64).div_ceil(BDL_ALIGN as u64) * BDL_ALIGN as u64;
            periods = ring.div_ceil(period).max(BDL_MIN as u64);
        }
        let ring_bytes = periods.checked_mul(period).filter(|&b| b <= u32::MAX as u64 / 2)?;
        Some(Layout { frame_bytes: frame_bytes as u32, ring_bytes: ring_bytes as u32, period_bytes: period as u32, periods: periods as usize })
    }
    pub fn ring_frames(&self) -> u32 {
        self.ring_bytes / self.frame_bytes
    }
    pub fn period_frames(&self) -> u32 {
        self.period_bytes / self.frame_bytes
    }
    pub fn bdl_bytes(&self) -> usize {
        self.periods * BDLE_LEN
    }
}

/// Fills the BDL in `bdl` for the ring at `data` (device address): one entry per period, IOC on each when
/// period interrupts are wanted.
pub fn write_bdl<M: Mem>(bdl: &M, data: u64, l: &Layout, ioc: bool) -> bool {
    (0..l.periods).all(|i| {
        let a = data + i as u64 * l.period_bytes as u64;
        let o = i * BDLE_LEN;
        bdl.wr32(o, a as u32) && bdl.wr32(o + 4, (a >> 32) as u32) && bdl.wr32(o + 8, l.period_bytes) && bdl.wr32(o + 12, ioc as u32)
    }) && {
        bdl.barrier();
        true
    }
}

/// Run off, interrupts off, status cleared, then the stream reset (bounded: Linux polls 300 us each way; we
/// give 1 ms). False if the descriptor did not go through reset.
pub fn reset<H: Hw + ?Sized>(hw: &H, i: usize) -> bool {
    let b = sd(i);
    let ctl = hw.rd8(b + SD_CTL);
    hw.wr8(b + SD_CTL, ctl & !(SD_CTL_RUN | SD_CTL_INT));
    hw.wr8(b + SD_STS, SD_STS_MASK);
    until(hw, 1000, 3, false, || hw.rd8(b + SD_CTL) & SD_CTL_RUN == 0);
    hw.wr8(b + SD_CTL, hw.rd8(b + SD_CTL) | SD_CTL_SRST);
    let entered = until(hw, 1000, 3, false, || hw.rd8(b + SD_CTL) & SD_CTL_SRST != 0);
    hw.wr8(b + SD_CTL, hw.rd8(b + SD_CTL) & !SD_CTL_SRST);
    let left = until(hw, 1000, 3, false, || hw.rd8(b + SD_CTL) & SD_CTL_SRST == 0);
    entered && left
}

/// Programs a reset descriptor (Linux `stream_setup` order): tag (and traffic priority without snooping),
/// CBL, format, LVI, BDL address.
pub fn program<H: Hw + ?Sized>(hw: &H, i: usize, tag: u8, format: u16, bdl: u64, l: &Layout, tp: bool) {
    let b = sd(i);
    // The 32-bit control register without its status byte (writing status bits back would clear them).
    let mut ctl = hw.rd32(b + SD_CTL) & 0x00FF_FFFF & !(SD_CTL_TAG_MASK | SD_CTL_RUN as u32);
    ctl |= (tag as u32 & 0xF) << SD_CTL_TAG_SHIFT;
    if tp {
        ctl |= SD_CTL_TP;
    }
    hw.wr32(b + SD_CTL, ctl);
    hw.wr32(b + SD_CBL, l.ring_bytes);
    hw.wr16(b + SD_FORMAT, format);
    hw.wr16(b + SD_LVI, (l.periods - 1) as u16);
    hw.wr32(b + SD_BDLPL, bdl as u32);
    hw.wr32(b + SD_BDLPU, (bdl >> 32) as u32);
}

/// Run bit on (with the completion interrupt when `ioc`) or off.
pub fn run<H: Hw + ?Sized>(hw: &H, i: usize, on: bool, ioc: bool) {
    let b = sd(i);
    let ctl = hw.rd8(b + SD_CTL) & !(SD_CTL_RUN | SD_CTL_INT);
    hw.wr8(b + SD_STS, SD_STS_MASK);
    let ints = if ioc { SD_CTL_IOCE | SD_CTL_FEIE | SD_CTL_DEIE } else { 0 };
    hw.wr8(b + SD_CTL, if on { ctl | SD_CTL_RUN | ints } else { ctl });
}

/// LPIB as a ring offset: below CBL as it is; CBL itself is the wrap (AMD's controller shows it right at the
/// wrap, V1 `ring.asm:29-31`); above is nonsense (all ones from a controller that is gone): None.
pub fn lpib_offset(raw: u32, ring_bytes: u32) -> Option<u32> {
    match raw {
        r if r < ring_bytes => Some(r),
        r if r == ring_bytes => Some(0),
        _ => None,
    }
}

/// The played bytes counted on from LPIB readings (it must be read at least once per ring, which the
/// service thread's bounded waits do).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    pub last: u32,
    pub played: u64,
    /// LPIB readings that made no sense (counted, the position stays).
    pub bogus: u64,
}

impl Position {
    /// Takes one LPIB reading; returns the bytes played since the last one.
    pub fn update(&mut self, raw: u32, ring_bytes: u32) -> u32 {
        let Some(now) = lpib_offset(raw, ring_bytes) else {
            self.bogus += 1;
            return 0;
        };
        let d = if now >= self.last { now - self.last } else { now + ring_bytes - self.last };
        self.last = now;
        self.played += d as u64;
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        let l = Layout::new(4, 16384, 4096).unwrap();
        assert_eq!((l.ring_bytes, l.period_bytes, l.periods), (65536, 16384, 4));
        // Periods round up to 128 bytes, the ring to whole periods.
        let l = Layout::new(4, 1000, 100).unwrap();
        assert_eq!(l.period_bytes, 512);
        assert_eq!(l.ring_bytes % 512, 0);
        assert!(l.ring_frames() >= 1000);
        // More than 256 periods asked: longer periods.
        let l = Layout::new(4, 48000, 32).unwrap();
        assert!(l.periods <= BDL_MAX && l.ring_frames() >= 48000);
        // Tiny rings still get two periods.
        let l = Layout::new(4, 1, 1).unwrap();
        assert_eq!((l.periods, l.period_bytes), (2, 128));
        assert_eq!(Layout::new(0, 10, 1), None);
        assert_eq!(Layout::new(3, 10, 1), None);
        assert_eq!(Layout::new(4, 0, 1), None);
        assert_eq!(Layout::new(8, u32::MAX, 1), None);
    }

    #[test]
    fn lpib_wrap_quirk() {
        assert_eq!(lpib_offset(0, 4096), Some(0));
        assert_eq!(lpib_offset(4095, 4096), Some(4095));
        assert_eq!(lpib_offset(4096, 4096), Some(0));
        assert_eq!(lpib_offset(4097, 4096), None);
        assert_eq!(lpib_offset(!0, 4096), None);
        let mut p = Position::default();
        assert_eq!(p.update(1000, 4096), 1000);
        assert_eq!(p.update(4096, 4096), 3096); // CBL at the wrap = 0
        assert_eq!(p.update(100, 4096), 100);
        assert_eq!(p.update(!0, 4096), 0);
        assert_eq!(p.update(50, 4096), 4046); // wrapped
        assert_eq!((p.played, p.bogus), (1000 + 3096 + 100 + 4046, 1));
    }
}
