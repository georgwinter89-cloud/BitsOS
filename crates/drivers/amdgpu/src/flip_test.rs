//! GX-3's hardware checks as code (generic over the register owner and clock, so the host tests run them on the
//! fake engine): `pace` = V1's FlipPace (a vertical bar moving fast, one flip per frame: 180 flips take 3 s, none
//! late; with `copy` the bar is copied into the shown buffer instead, which tears: the before/after picture),
//! `crc` = the OTG CRC follows the buffer shown (V1: "changes with A1 and returns with A0").

use crate::flip::{self, Rw};
use crate::scanout::{Clock, Scanout};
use alloc::vec::Vec;
use devmodel::display::DisplayDevice;
use gopfb::fbdev::flip::FlipError;

/// The bar: 32 pixels wide, 48 pixels further each frame (about 2.9 s across 2560 pixels at 60 Hz).
pub const BAR_W: u32 = 32;
pub const BAR_STEP: u32 = 48;
const BAR: u32 = 0x00FF_FFFF;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pace {
    pub flips: u64,
    pub ns: u64,
    /// Flips whose landing the frame counter saw 0, 1 or 2+ vblanks after the order.
    pub after: [u64; 3],
    /// Flips that did not land in time (the surface fell back) or could not be ordered.
    pub late: u64,
    pub error: Option<FlipError>,
}

impl Pace {
    pub fn text(&self, copy: bool, period_ns: u64) -> alloc::string::String {
        let ms = self.ns / 1_000_000;
        let expect = self.flips * period_ns / 1_000_000;
        if copy {
            return alloc::format!("pace (copy into the shown buffer, tears): {} frames in {ms} ms (one per vblank: {expect} ms)", self.flips);
        }
        alloc::format!(
            "pace: {} flips in {ms} ms (one per frame: {expect} ms), late {}, landed after 0/1/2+ vblanks: {}/{}/{}{}",
            self.flips,
            self.late,
            self.after[0],
            self.after[1],
            self.after[2],
            self.error.map_or(alloc::string::String::new(), |e| alloc::format!(", stopped: {e:?}"))
        )
    }
}

fn bar(i: u64, w: u32, h: u32) -> (u32, u32, u32, u32) {
    let span = w.saturating_sub(BAR_W).max(1);
    let x = (i.wrapping_mul(u64::from(BAR_STEP)) % u64::from(span)) as u32;
    (x, 0, BAR_W.min(w), h)
}

fn fill(src: &mut [u32], w: u32, r: (u32, u32, u32, u32), from: Option<&[u32]>) {
    for y in r.1..r.1 + r.3 {
        let a = (y * w + r.0) as usize;
        let b = a + r.2 as usize;
        match from {
            Some(base) => src[a..b].copy_from_slice(&base[a..b]),
            None => src[a..b].fill(BAR),
        }
    }
}

fn union(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> (u32, u32, u32, u32) {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    (x0, y0, x1 - x0, y1 - y0)
}

/// `flips` frames of the moving bar over `base` (a full `w` x `h` frame); `src` is scratch of the same size and
/// ends equal to `base`'s picture with the last bar. `copy`: present into the shown buffer and sleep to the next
/// vblank (the old path). `stop` is asked between frames (the MCP call's stop).
pub fn pace<R: Rw, C: Clock>(
    s: &Scanout<R, C>,
    clock: &dyn Fn() -> u64,
    sleep: &dyn Fn(u64),
    base: &[u32],
    src: &mut [u32],
    flips: u64,
    copy: bool,
    stop: &dyn Fn() -> bool,
) -> Pace {
    let (w, h) = s.size();
    let mut p = Pace::default();
    let t0 = clock();
    let mut prev: Option<(u32, u32, u32, u32)> = None;
    for i in 0..flips {
        if stop() {
            break;
        }
        let r = bar(i, w, h);
        if let Some(q) = prev {
            fill(src, w, q, Some(base));
        }
        fill(src, w, r, None);
        // The first frame is whole: a new picture source (the marks only know what changed since).
        let dirty = prev.map_or((0, 0, w, h), |q| union(q, r));
        prev = Some(r);
        if copy {
            let _ = s.present_shown(src, w, dirty);
            p.flips += 1;
            if let Some(t) = s.next_vblank_ns() {
                sleep(t.saturating_sub(clock()));
            }
            continue;
        }
        let back = match s.begin(src, w) {
            Ok(b) => b,
            Err(e) => {
                p.late += u64::from(e == FlipError::Late);
                p.error = Some(e);
                break;
            }
        };
        let _ = back.present(src, w, dirty);
        let f0 = s.vblank().map_or(0, |v| v.last.sequence);
        let seq = match s.commit() {
            Ok(q) => q,
            Err(e) => {
                p.late += 1;
                p.error = Some(e);
                break;
            }
        };
        if let Err(e) = s.wait(seq) {
            p.late += 1;
            p.error = Some(e);
            break;
        }
        let f1 = s.vblank().map_or(0, |v| v.last.sequence);
        p.after[(f1.saturating_sub(f0)).min(2) as usize] += 1;
        p.flips += 1;
    }
    p.ns = clock().saturating_sub(t0);
    p
}

/// One CRC reading: which buffer was shown and the CRC of the frames after it landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CrcRead {
    pub buffer: u8,
    pub crc: Option<u64>,
}

/// `rounds` times: `q` into the hidden buffer, flipped, two frames later the CRC; then `p` the same way. The CRC
/// is switched on for the run and put back as found (`flip::crc_on`/`crc_undo`).
pub fn crc<R: Rw, C: Clock>(
    s: &Scanout<R, C>,
    sleep: &dyn Fn(u64),
    p: &[u32],
    q: &[u32],
    rounds: u32,
) -> Result<Vec<CrcRead>, alloc::string::String> {
    let (w, h) = s.size();
    let k = s.otg();
    let saved = flip::crc_on(s.regs(), k, w, h).map_err(crate::scanout::reg_error_text)?;
    let mut out = Vec::new();
    let mut res = Ok(());
    'r: for _ in 0..rounds {
        for f in [q, p] {
            let shown = match s.begin(f, w) {
                Ok(b) => {
                    let _ = b.present(f, w, (0, 0, w, h));
                    s.commit().and_then(|seq| s.wait(seq))
                }
                Err(e) => Err(e),
            };
            if let Err(e) = shown {
                res = Err(alloc::format!("flip: {e:?}"));
                break 'r;
            }
            sleep(2 * s.timing().period_ns());
            let crc = flip::crc_read(s.regs(), k).map_err(crate::scanout::reg_error_text);
            match crc {
                Ok(c) => out.push(CrcRead { buffer: s.state().shown, crc: c }),
                Err(e) => {
                    res = Err(e);
                    break 'r;
                }
            }
        }
    }
    if let Some(sv) = saved {
        flip::crc_undo(s.regs(), k, &sv).map_err(crate::scanout::reg_error_text)?;
    }
    res.map(|()| out)
}

/// The CRC verdict: the two buffers' CRCs differ and each is the same every round.
pub fn crc_text(reads: &[CrcRead]) -> alloc::string::String {
    let of = |b: u8| reads.iter().filter(|r| r.buffer == b).filter_map(|r| r.crc).collect::<Vec<_>>();
    let (a0, a1) = (of(0), of(1));
    let stable = |v: &[u64]| !v.is_empty() && v.iter().all(|&c| c == v[0]);
    let differ = stable(&a0) && stable(&a1) && a0[0] != a1[0];
    let list = |v: &[u64]| v.iter().map(|c| alloc::format!("{c:#014x}")).collect::<Vec<_>>().join(" ");
    alloc::format!(
        "crc: A1 (inverted picture) {}; A0 (picture) {}; stable per buffer {}; changes with the flip {}",
        list(&a1),
        list(&a0),
        stable(&a0) && stable(&a1),
        differ
    )
}
