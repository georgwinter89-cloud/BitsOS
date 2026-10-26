//! Rational polyphase resampler (client rate -> the mixer's 48 kHz), integer only in the mixing path.
//!
//! Up by L, low-pass, down by M with L/M = 48000/rate in lowest terms (44.1 kHz: 160/147). The prototype is a
//! Kaiser-windowed sinc of L*T taps at L*rate (T = 64 taps per phase, T*ceil(M/L) when going down); cut-off
//! (-6 dB) at 0.975 * min(rate, 48000) / 2 (21.5 kHz for 44.1 kHz), beta 9 (about 90 dB stop band, ~4 kHz
//! transition): flat to 20 kHz, and an image lands either above 22 kHz (inaudible) or far in the stop band
//! (content f images at 44.1k - f, folded to f + 3.9k at 48 kHz only once 44.1k - f > 24k). Each phase
//! is scaled to a DC gain of exactly 1 (no phase ripple) and kept as Q20 i32; the sum runs in i64 and is
//! rounded and clamped to 16 bit. Output n uses input i = floor(n*M/L) and phase (n*M) mod L:
//! y[n] = sum_k h[k*L + phase] * x[i - k] (times L for the zero stuffing). Delay: T/2 input frames.
//! Measured THD+N (host tests, `cargo test -p snd -- --nocapture thd`): see plan/notes/sn-SN-3.md.

use crate::fmath;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;

pub const OUT_RATE: u32 = 48000;
pub const COEF_SHIFT: u32 = 20;
const TAPS: usize = 64;
const BETA: f64 = 9.0;
const CUTOFF: f64 = 0.975;

/// The rates a client may use (ALSA's standard list). Everything else is refused: an odd rate would need
/// thousands of phases.
pub const RATES: [u32; 11] = [8000, 11025, 16000, 22050, 32000, 44100, 48000, 88200, 96000, 176400, 192000];

pub struct Table {
    pub rate: u32,
    pub l: u32,
    pub m: u32,
    pub taps: usize,
    /// [phase][tap], Q20.
    pub coef: Vec<i32>,
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

impl Table {
    /// None if the memory for the prototype or the coefficients cannot be had.
    pub fn build(rate: u32) -> Option<Table> {
        let g = gcd(OUT_RATE, rate);
        let (l, m) = (OUT_RATE / g, rate / g);
        let taps = TAPS * m.div_ceil(l).max(1) as usize;
        let n = l as usize * taps;
        // Cut-off in cycles per sample at the upsampled rate l*rate.
        let fc = CUTOFF * (rate.min(OUT_RATE) as f64 / 2.0) / (l as f64 * rate as f64);
        let mid = (n as f64 - 1.0) / 2.0;
        let i0b = fmath::bessel_i0(BETA);
        let proto = |j: usize| {
            let x = j as f64 - mid;
            let r = 2.0 * j as f64 / (n as f64 - 1.0) - 1.0;
            let w = fmath::bessel_i0(BETA * fmath::sqrt(1.0 - r * r)) / i0b;
            2.0 * fc * fmath::sinc(2.0 * fc * x) * w
        };
        let mut h: Vec<f64> = Vec::new();
        h.try_reserve_exact(n).ok()?;
        h.extend((0..n).map(proto));
        let one = (1i64 << COEF_SHIFT) as f64;
        let mut coef: Vec<i32> = Vec::new();
        coef.try_reserve_exact(n).ok()?;
        coef.resize(n, 0);
        for p in 0..l as usize {
            let sum: f64 = (0..taps).map(|k| h[k * l as usize + p]).sum();
            let row = &mut coef[p * taps..(p + 1) * taps];
            let mut total: i64 = 0;
            for (k, c) in row.iter_mut().enumerate() {
                let v = h[k * l as usize + p] / sum * one;
                *c = fmath::floor(v + 0.5) as i32;
                total += *c as i64;
            }
            // Exactly unity at DC: the rounding error goes into the largest tap.
            let big = (0..taps).max_by_key(|&k| row[k].unsigned_abs()).unwrap_or(0);
            row[big] += ((1i64 << COEF_SHIFT) - total) as i32;
        }
        Some(Table { rate, l, m, taps, coef })
    }

    pub fn phases(&self) -> u32 {
        self.l
    }
}

static TABLES: SpinLock<Vec<Arc<Table>>> = SpinLock::named("snd.tables", Vec::new());

/// The shared table for `rate` (built on first use, outside the lock; kept for good, one per rate). None:
/// no memory for it.
pub fn table(rate: u32) -> Option<Arc<Table>> {
    if let Some(t) = TABLES.lock().iter().find(|t| t.rate == rate) {
        return Some(t.clone());
    }
    let t0 = time::now_ns();
    let Some(built) = Table::build(rate) else {
        klog::event!(Warn, "snd", "mix.table_nomem", rate = rate);
        return None;
    };
    let t = Arc::new(built);
    let mut g = TABLES.lock();
    if let Some(old) = g.iter().find(|x| x.rate == rate) {
        return Some(old.clone());
    }
    if g.try_reserve(1).is_err() {
        // Not cached: still usable by this client.
        return Some(t);
    }
    klog::event!(Info, "snd", "mix.table", rate = rate, phases = t.l, step = t.m, taps = t.taps as u64,
        build_us = time::now_ns().saturating_sub(t0) / 1000);
    g.push(t.clone());
    Some(t)
}

/// One client's filter state.
pub struct Resampler {
    t: Arc<Table>,
    /// The last `taps` input frames, stored twice so `win[at..at + taps]` is newest-first and contiguous.
    win: Vec<[i32; 2]>,
    at: usize,
    phase: u32,
    /// Inputs to take before the next output.
    need: u32,
}

impl Resampler {
    /// None if the window cannot be had.
    pub fn try_new(t: Arc<Table>) -> Option<Resampler> {
        let n = 2 * t.taps;
        let mut win = Vec::new();
        win.try_reserve_exact(n).ok()?;
        win.resize(n, [0; 2]);
        Some(Resampler { t, win, at: 0, phase: 0, need: 1 })
    }

    /// Forgets the history (ALSA drop): as new, without allocating.
    pub fn reset(&mut self) {
        self.win.fill([0; 2]);
        self.at = 0;
        self.phase = 0;
        self.need = 1;
    }

    pub fn taps(&self) -> usize {
        self.t.taps
    }

    /// Input frames `run` takes to make `n` outputs.
    pub fn inputs_for(&self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let (l, m) = (self.t.l as u64, self.t.m as u64);
        (self.need as u64 + (self.phase as u64 + (n as u64 - 1) * m) / l) as usize
    }

    /// The most outputs `avail` input frames make (the inverse of `inputs_for`).
    pub fn outputs_for(&self, avail: usize) -> usize {
        let (l, m) = (self.t.l as u64, self.t.m as u64);
        let Some(k) = (avail as u64).checked_sub(self.need as u64) else { return 0 };
        (((k + 1) * l - 1 - self.phase as u64) / m + 1) as usize
    }

    fn push(&mut self, s: [i16; 2]) {
        let taps = self.t.taps;
        self.at = if self.at == 0 { taps - 1 } else { self.at - 1 };
        let v = [s[0] as i32, s[1] as i32];
        self.win[self.at] = v;
        self.win[self.at + taps] = v;
    }

    /// `input.len()` must be `inputs_for(out.len())` (missing inputs count as silence).
    pub fn run(&mut self, input: &[[i16; 2]], out: &mut [[i32; 2]]) {
        let (l, m, taps) = (self.t.l, self.t.m, self.t.taps);
        let t = self.t.clone();
        let mut ii = 0;
        let round = 1i64 << (COEF_SHIFT - 1);
        for o in out.iter_mut() {
            for _ in 0..self.need {
                self.push(input.get(ii).copied().unwrap_or([0, 0]));
                ii += 1;
            }
            let p = self.phase as usize;
            let c = &t.coef[p * taps..(p + 1) * taps];
            let w = &self.win[self.at..self.at + taps];
            let (mut a, mut b) = (0i64, 0i64);
            for (k, x) in c.iter().zip(w) {
                a += *k as i64 * x[0] as i64;
                b += *k as i64 * x[1] as i64;
            }
            *o = [clamp16((a + round) >> COEF_SHIFT), clamp16((b + round) >> COEF_SHIFT)];
            let next = self.phase + m;
            self.need = next / l;
            self.phase = next % l;
        }
    }
}

pub fn clamp16(v: i64) -> i32 {
    v.clamp(i16::MIN as i64, i16::MAX as i64) as i32
}
