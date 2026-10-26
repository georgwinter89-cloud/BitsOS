//! `snd_pcm_hw_params` and Linux's refine engine (`linux-6.6/sound/core/pcm_native.c:284-647`, the interval
//! arithmetic of `pcm_lib.c:539-800,1030,1617-1700`, `include/sound/pcm_params.h`), so libasound's hw_params
//! search (set_*_near, plug's choices) ends where it ends on Linux. Kernel-free; host tests drive it.

use super::abi::*;

pub const ACCESS: usize = 0;
pub const FORMAT: usize = 1;
pub const SUBFORMAT: usize = 2;
pub const SAMPLE_BITS: usize = 8;
pub const FRAME_BITS: usize = 9;
pub const CHANNELS: usize = 10;
pub const RATE: usize = 11;
pub const PERIOD_TIME: usize = 12;
pub const PERIOD_SIZE: usize = 13;
pub const PERIOD_BYTES: usize = 14;
pub const PERIODS: usize = 15;
pub const BUFFER_TIME: usize = 16;
pub const BUFFER_SIZE: usize = 17;
pub const BUFFER_BYTES: usize = 18;
pub const TICK_TIME: usize = 19;
const FIRST_INTERVAL: usize = SAMPLE_BITS;
const LAST: usize = TICK_TIME;

const EINVAL: i64 = uapi::errno::EINVAL;

/// `struct snd_interval`; `bits` keeps the flag word's other bits as the program gave them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interval {
    pub min: u32,
    pub max: u32,
    pub openmin: bool,
    pub openmax: bool,
    pub integer: bool,
    pub empty: bool,
    pub bits: u32,
}

impl Interval {
    pub const fn any() -> Interval {
        Interval { min: 0, max: u32::MAX, openmin: false, openmax: false, integer: false, empty: false, bits: 0 }
    }
    pub fn range(min: u32, max: u32) -> Interval {
        Interval { min, max, ..Interval::any() }
    }
    pub fn single(&self) -> bool {
        self.min == self.max || (self.min.wrapping_add(1) == self.max && (self.openmin || self.openmax))
    }
    /// `snd_interval_value`.
    pub fn value(&self) -> u32 {
        if self.openmin && !self.openmax { self.max } else { self.min }
    }
    fn checkempty(&self) -> bool {
        self.min > self.max || (self.min == self.max && (self.openmin || self.openmax))
    }
    pub fn test(&self, v: u32) -> bool {
        !(self.min > v || (self.min == v && self.openmin) || self.max < v || (self.max == v && self.openmax))
    }
    /// `snd_interval_refine` (pcm_lib.c:606).
    pub fn refine(&mut self, v: &Interval) -> Result<bool, i64> {
        if self.empty {
            return Err(EINVAL);
        }
        let mut changed = false;
        if self.min < v.min {
            self.min = v.min;
            self.openmin = v.openmin;
            changed = true;
        } else if self.min == v.min && !self.openmin && v.openmin {
            self.openmin = true;
            changed = true;
        }
        if self.max > v.max {
            self.max = v.max;
            self.openmax = v.openmax;
            changed = true;
        } else if self.max == v.max && !self.openmax && v.openmax {
            self.openmax = true;
            changed = true;
        }
        if !self.integer && v.integer {
            self.integer = true;
            changed = true;
        }
        if self.integer {
            if self.openmin {
                self.min = self.min.wrapping_add(1);
                self.openmin = false;
            }
            if self.openmax {
                self.max = self.max.wrapping_sub(1);
                self.openmax = false;
            }
        } else if !self.openmin && !self.openmax && self.min == self.max {
            self.integer = true;
        }
        if self.checkempty() {
            self.empty = true;
            return Err(EINVAL);
        }
        Ok(changed)
    }
    fn refine_first(&mut self) -> Result<bool, i64> {
        if self.empty {
            return Err(EINVAL);
        }
        if self.single() {
            return Ok(false);
        }
        let last_max = self.max;
        self.max = self.min;
        if self.openmin {
            self.max = self.max.wrapping_add(1);
        }
        self.openmax = self.openmax && self.max >= last_max;
        Ok(true)
    }
    fn refine_last(&mut self) -> Result<bool, i64> {
        if self.empty {
            return Err(EINVAL);
        }
        if self.single() {
            return Ok(false);
        }
        let last_min = self.min;
        self.min = self.max;
        if self.openmax {
            self.min = self.min.wrapping_sub(1);
        }
        self.openmin = self.openmin && self.min <= last_min;
        Ok(true)
    }
    fn none() -> Interval {
        Interval { empty: true, ..Interval::any() }
    }
    fn setinteger(&mut self) {
        self.integer = true;
    }
}

fn div32(a: u32, b: u32) -> (u32, u32) {
    if b == 0 { (u32::MAX, 0) } else { (a / b, a % b) }
}
fn mul(a: u32, b: u32) -> u32 {
    a.checked_mul(b).unwrap_or(u32::MAX)
}
fn muldiv32(a: u32, b: u32, c: u32) -> (u32, u32) {
    if c == 0 {
        return (u32::MAX, 0);
    }
    let n = a as u64 * b as u64;
    let (q, r) = (n / c as u64, (n % c as u64) as u32);
    if q >= u32::MAX as u64 { (u32::MAX, 0) } else { (q as u32, r) }
}

fn i_mul(a: &Interval, b: &Interval) -> Interval {
    if a.empty || b.empty {
        return Interval::none();
    }
    Interval { min: mul(a.min, b.min), openmin: a.openmin || b.openmin, max: mul(a.max, b.max), openmax: a.openmax || b.openmax, integer: a.integer && b.integer, ..Interval::any() }
}
fn i_div(a: &Interval, b: &Interval) -> Interval {
    if a.empty || b.empty {
        return Interval::none();
    }
    let mut c = Interval::any();
    let (q, r) = div32(a.min, b.max);
    c.min = q;
    c.openmin = r != 0 || a.openmin || b.openmax;
    if b.min > 0 {
        let (q, r) = div32(a.max, b.min);
        c.max = q;
        if r != 0 {
            c.max = c.max.wrapping_add(1);
            c.openmax = true;
        } else {
            c.openmax = a.openmax || b.openmin;
        }
    } else {
        c.max = u32::MAX;
        c.openmax = false;
    }
    c
}
fn i_muldivk(a: &Interval, b: &Interval, k: u32) -> Interval {
    if a.empty || b.empty {
        return Interval::none();
    }
    let mut c = Interval::any();
    let (q, r) = muldiv32(a.min, b.min, k);
    c.min = q;
    c.openmin = r != 0 || a.openmin || b.openmin;
    let (q, r) = muldiv32(a.max, b.max, k);
    c.max = q;
    if r != 0 {
        c.max = c.max.wrapping_add(1);
        c.openmax = true;
    } else {
        c.openmax = a.openmax || b.openmax;
    }
    c
}
fn i_mulkdiv(a: &Interval, k: u32, b: &Interval) -> Interval {
    if a.empty || b.empty {
        return Interval::none();
    }
    let mut c = Interval::any();
    let (q, r) = muldiv32(a.min, k, b.max);
    c.min = q;
    c.openmin = r != 0 || a.openmin || b.openmax;
    if b.min > 0 {
        let (q, r) = muldiv32(a.max, k, b.min);
        c.max = q;
        if r != 0 {
            c.max = c.max.wrapping_add(1);
            c.openmax = true;
        } else {
            c.openmax = a.openmax || b.openmin;
        }
    } else {
        c.max = u32::MAX;
        c.openmax = false;
    }
    c
}

/// The kernel looks at the first 64 bits of a mask (`SNDRV_MASK_BITS`, pcm_params.h).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mask(pub [u32; 8]);

impl Mask {
    pub fn of(bits: &[u32]) -> Mask {
        let mut m = [0u32; 8];
        for &b in bits {
            m[(b >> 5) as usize] |= 1 << (b & 31);
        }
        Mask(m)
    }
    fn empty(&self) -> bool {
        self.0[0] == 0 && self.0[1] == 0
    }
    pub fn test(&self, v: u32) -> bool {
        v < 64 && self.0[(v >> 5) as usize] & (1 << (v & 31)) != 0
    }
    pub fn min(&self) -> u32 {
        for i in 0..2 {
            if self.0[i] != 0 {
                return self.0[i].trailing_zeros() + (i as u32) * 32;
            }
        }
        0
    }
    fn max(&self) -> u32 {
        for i in (0..2).rev() {
            if self.0[i] != 0 {
                return 31 - self.0[i].leading_zeros() + (i as u32) * 32;
            }
        }
        0
    }
    pub fn single(&self) -> bool {
        let mut c = 0;
        for i in 0..2 {
            let b = self.0[i];
            if b == 0 {
                continue;
            }
            if b & (b - 1) != 0 || c != 0 {
                return false;
            }
            c += 1;
        }
        true
    }
    fn reset(&mut self, v: u32) {
        self.0[(v >> 5) as usize] &= !(1 << (v & 31));
    }
    fn leave(&mut self, v: u32) {
        let keep = self.0[(v >> 5) as usize] & (1 << (v & 31));
        self.0 = [0; 8];
        self.0[(v >> 5) as usize] = keep;
    }
    /// `snd_mask_refine`: intersect (64 bits), EINVAL when nothing is left.
    fn refine(&mut self, v: &Mask) -> Result<bool, i64> {
        let old = *self;
        self.0[0] &= v.0[0];
        self.0[1] &= v.0[1];
        if self.empty() {
            return Err(EINVAL);
        }
        Ok(self.0[0] != old.0[0] || self.0[1] != old.0[1])
    }
    fn refine_first(&mut self) -> bool {
        if self.single() {
            return false;
        }
        self.leave(self.min());
        true
    }
    fn refine_last(&mut self) -> bool {
        if self.single() {
            return false;
        }
        self.leave(self.max());
        true
    }
}

/// `snd_pcm_format_physical_width` (pcm_misc.c), by format number; 0 = no width (MPEG, GSM, SPECIAL).
pub fn phys_width(f: u32) -> u32 {
    match f {
        0 | 1 | 20 | 21 | 45 | 47 | 48 => 8,
        2..=5 | 49 | 51 => 16,
        6..=15 | 18 | 19 | 25..=28 | 50 | 52 => 32,
        16 | 17 => 64,
        22 => 4,
        32..=43 => 24,
        44 => 3,
        46 => 5,
        _ => 0,
    }
}

/// `struct snd_pcm_hw_params` decoded (offsets: abi's sizes; masks at 4, intervals at 260, rmask 512, cmask
/// 516, info 520, msbits 524, rate_num 528, rate_den 532, fifo_size 536). `raw` keeps the bytes the program
/// gave (reserved masks and intervals go back unchanged, as Linux copies its whole struct back).
#[derive(Clone)]
pub struct HwParams {
    pub raw: [u8; HW_PARAMS_SIZE],
    pub flags: u32,
    pub masks: [Mask; 3],
    pub iv: [Interval; LAST - FIRST_INTERVAL + 1],
    pub rmask: u32,
    pub cmask: u32,
    pub info: u32,
    pub msbits: u32,
    pub rate_num: u32,
    pub rate_den: u32,
    pub fifo_size: u64,
}

const MASKS_AT: usize = 4;
const IV_AT: usize = 260;

impl HwParams {
    pub fn decode(raw: &[u8; HW_PARAMS_SIZE]) -> HwParams {
        let mut masks = [Mask([0; 8]); 3];
        for (k, m) in masks.iter_mut().enumerate() {
            for w in 0..8 {
                m.0[w] = get_u32(raw, MASKS_AT + k * 32 + w * 4);
            }
        }
        let mut iv = [Interval::any(); LAST - FIRST_INTERVAL + 1];
        for (k, i) in iv.iter_mut().enumerate() {
            let at = IV_AT + k * 12;
            let f = get_u32(raw, at + 8);
            *i = Interval { min: get_u32(raw, at), max: get_u32(raw, at + 4), openmin: f & 1 != 0, openmax: f & 2 != 0, integer: f & 4 != 0, empty: f & 8 != 0, bits: f & !0xf };
        }
        HwParams {
            raw: *raw,
            flags: get_u32(raw, 0),
            masks,
            iv,
            rmask: get_u32(raw, 512),
            cmask: get_u32(raw, 516),
            info: get_u32(raw, 520),
            msbits: get_u32(raw, 524),
            rate_num: get_u32(raw, 528),
            rate_den: get_u32(raw, 532),
            fifo_size: get_u64(raw, 536),
        }
    }
    pub fn encode(&self) -> [u8; HW_PARAMS_SIZE] {
        let mut b = self.raw;
        put_u32(&mut b, 0, self.flags);
        for (k, m) in self.masks.iter().enumerate() {
            for w in 0..8 {
                put_u32(&mut b, MASKS_AT + k * 32 + w * 4, m.0[w]);
            }
        }
        for (k, i) in self.iv.iter().enumerate() {
            let at = IV_AT + k * 12;
            put_u32(&mut b, at, i.min);
            put_u32(&mut b, at + 4, i.max);
            let f = i.bits | i.openmin as u32 | (i.openmax as u32) << 1 | (i.integer as u32) << 2 | (i.empty as u32) << 3;
            put_u32(&mut b, at + 8, f);
        }
        put_u32(&mut b, 512, self.rmask);
        put_u32(&mut b, 516, self.cmask);
        put_u32(&mut b, 520, self.info);
        put_u32(&mut b, 524, self.msbits);
        put_u32(&mut b, 528, self.rate_num);
        put_u32(&mut b, 532, self.rate_den);
        put_u64(&mut b, 536, self.fifo_size);
        b
    }
    /// `_snd_pcm_hw_params_any`: everything allowed, every parameter requested (tests, the C check's model).
    pub fn any() -> HwParams {
        let mut p = HwParams::decode(&[0; HW_PARAMS_SIZE]);
        for m in p.masks.iter_mut() {
            m.0[0] = !0;
            m.0[1] = !0;
        }
        p.iv = [Interval::any(); LAST - FIRST_INTERVAL + 1];
        p.rmask = !0;
        p.info = !0;
        p
    }
    pub fn mask(&self, k: usize) -> &Mask {
        &self.masks[k]
    }
    pub fn iv(&self, k: usize) -> &Interval {
        &self.iv[k - FIRST_INTERVAL]
    }
    pub fn iv_mut(&mut self, k: usize) -> &mut Interval {
        &mut self.iv[k - FIRST_INTERVAL]
    }
    /// `params_*()`: the minimum of an interval / the lowest bit of a mask.
    pub fn get(&self, k: usize) -> u32 {
        if k <= SUBFORMAT { self.masks[k].min() } else { self.iv(k).min }
    }
}

/// The device's `snd_pcm_hardware` (what the mixer takes, SN-3) plus the driver's own constraints.
#[derive(Clone, Copy, Debug)]
pub struct Hw {
    pub info: u32,
    pub formats: u64,
    /// `SNDRV_PCM_RATE_*` bits over the known-rates list.
    pub rates: u32,
    pub rate_min: u32,
    pub rate_max: u32,
    pub channels_min: u32,
    pub channels_max: u32,
    pub buffer_bytes_max: u32,
    pub period_bytes_min: u32,
    pub period_bytes_max: u32,
    pub periods_min: u32,
    pub periods_max: u32,
    /// `snd_pcm_hw_constraint_msbits(runtime, 0, 32, msbits)`: 32-bit samples carry this many bits.
    pub msbits32: u32,
}

/// `snd_pcm_known_rates` (pcm_native.c:2448).
pub const KNOWN_RATES: [u32; 15] = [5512, 8000, 11025, 16000, 22050, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000, 352800, 384000];

#[derive(Clone, Copy)]
enum Func {
    Format,
    SampleBits,
    Mul,
    Div,
    MulDivK(u32),
    MulKDiv(u32),
    Rate,
    BufferBytesMax,
    MsBits,
}

#[derive(Clone, Copy)]
struct Rule {
    /// The parameter it changes; None = it only sets `msbits`.
    var: Option<usize>,
    func: Func,
    deps: &'static [usize],
}

const fn r(var: usize, func: Func, deps: &'static [usize]) -> Rule {
    Rule { var: Some(var), func, deps }
}

/// `snd_pcm_hw_constraints_init` (pcm_native.c:2486), `_complete` (:2610) and the driver's msbits rule, in
/// their order (the order matters: Linux applies them in sequence until nothing changes).
const RULES: [Rule; 23] = [
    r(FORMAT, Func::Format, &[SAMPLE_BITS]),
    r(SAMPLE_BITS, Func::SampleBits, &[FORMAT, SAMPLE_BITS]),
    r(SAMPLE_BITS, Func::Div, &[FRAME_BITS, CHANNELS]),
    r(FRAME_BITS, Func::Mul, &[SAMPLE_BITS, CHANNELS]),
    r(FRAME_BITS, Func::MulKDiv(8), &[PERIOD_BYTES, PERIOD_SIZE]),
    r(FRAME_BITS, Func::MulKDiv(8), &[BUFFER_BYTES, BUFFER_SIZE]),
    r(CHANNELS, Func::Div, &[FRAME_BITS, SAMPLE_BITS]),
    r(RATE, Func::MulKDiv(1_000_000), &[PERIOD_SIZE, PERIOD_TIME]),
    r(RATE, Func::MulKDiv(1_000_000), &[BUFFER_SIZE, BUFFER_TIME]),
    r(PERIODS, Func::Div, &[BUFFER_SIZE, PERIOD_SIZE]),
    r(PERIOD_SIZE, Func::Div, &[BUFFER_SIZE, PERIODS]),
    r(PERIOD_SIZE, Func::MulKDiv(8), &[PERIOD_BYTES, FRAME_BITS]),
    r(PERIOD_SIZE, Func::MulDivK(1_000_000), &[PERIOD_TIME, RATE]),
    r(BUFFER_SIZE, Func::Mul, &[PERIOD_SIZE, PERIODS]),
    r(BUFFER_SIZE, Func::MulKDiv(8), &[BUFFER_BYTES, FRAME_BITS]),
    r(BUFFER_SIZE, Func::MulDivK(1_000_000), &[BUFFER_TIME, RATE]),
    r(PERIOD_BYTES, Func::MulDivK(8), &[PERIOD_SIZE, FRAME_BITS]),
    r(BUFFER_BYTES, Func::MulDivK(8), &[BUFFER_SIZE, FRAME_BITS]),
    r(PERIOD_TIME, Func::MulKDiv(1_000_000), &[PERIOD_SIZE, RATE]),
    r(BUFFER_TIME, Func::MulKDiv(1_000_000), &[BUFFER_SIZE, RATE]),
    r(BUFFER_BYTES, Func::BufferBytesMax, &[BUFFER_BYTES]),
    r(RATE, Func::Rate, &[RATE]),
    Rule { var: None, func: Func::MsBits, deps: &[SAMPLE_BITS] },
];

/// The constraints of one device (`runtime->hw_constraints`).
pub struct Constraints {
    pub hw: Hw,
    masks: [Mask; 3],
    iv: [Interval; LAST - FIRST_INTERVAL + 1],
}

fn bit(k: usize) -> u32 {
    1 << k
}

impl Constraints {
    pub fn new(hw: Hw) -> Constraints {
        let mut iv = [Interval::any(); LAST - FIRST_INTERVAL + 1];
        for k in [CHANNELS, BUFFER_SIZE, BUFFER_BYTES, SAMPLE_BITS, FRAME_BITS, PERIOD_SIZE, PERIODS] {
            iv[k - FIRST_INTERVAL].setinteger();
        }
        let mut c = Constraints { hw, masks: [Mask([0; 8]); 3], iv };
        c.masks[ACCESS].0[0] = 1 << ACCESS_RW_INTERLEAVED;
        c.masks[FORMAT].0[0] = hw.formats as u32;
        c.masks[FORMAT].0[1] = (hw.formats >> 32) as u32;
        c.masks[SUBFORMAT].0[0] = 1 << SUBFORMAT_STD;
        for (k, lo, hi) in [
            (CHANNELS, hw.channels_min, hw.channels_max),
            (RATE, hw.rate_min, hw.rate_max),
            (PERIOD_BYTES, hw.period_bytes_min, hw.period_bytes_max),
            (PERIODS, hw.periods_min, hw.periods_max),
            (BUFFER_BYTES, hw.period_bytes_min, hw.buffer_bytes_max),
        ] {
            // Constant input: refine cannot fail on these ranges.
            let _ = c.iv[k - FIRST_INTERVAL].refine(&Interval::range(lo, hi));
        }
        c
    }

    fn apply(&self, p: &mut HwParams, rule: &Rule) -> Result<bool, i64> {
        let d = rule.deps;
        let t = match rule.func {
            Func::Mul => i_mul(p.iv(d[0]), p.iv(d[1])),
            Func::Div => i_div(p.iv(d[0]), p.iv(d[1])),
            Func::MulDivK(k) => i_muldivk(p.iv(d[0]), p.iv(d[1]), k),
            Func::MulKDiv(k) => i_mulkdiv(p.iv(d[0]), k, p.iv(d[1])),
            Func::Format => {
                let i = *p.iv(SAMPLE_BITS);
                let mut m = Mask([!0; 8]);
                for f in 0..64 {
                    let bits = phys_width(f);
                    if p.masks[FORMAT].test(f) && bits > 0 && (bits < i.min || bits > i.max) {
                        m.reset(f);
                    }
                }
                return p.masks[FORMAT].refine(&m);
            }
            Func::SampleBits => {
                let mut t = Interval { min: u32::MAX, max: 0, ..Interval::any() };
                for f in 0..64 {
                    let bits = phys_width(f);
                    if p.masks[FORMAT].test(f) && bits > 0 {
                        t.min = t.min.min(bits);
                        t.max = t.max.max(bits);
                    }
                }
                t.integer = true;
                t
            }
            Func::Rate => {
                // snd_interval_list over the known rates the device has (pcm_lib.c:1030).
                let i = *p.iv(RATE);
                let mut t = Interval { min: u32::MAX, max: 0, ..Interval::any() };
                for (k, &rate) in KNOWN_RATES.iter().enumerate() {
                    if self.hw.rates & (1 << k) == 0 || !i.test(rate) {
                        continue;
                    }
                    t.min = t.min.min(rate);
                    t.max = t.max.max(rate);
                }
                t
            }
            Func::BufferBytesMax => Interval { min: 0, max: self.hw.buffer_bytes_max, integer: true, ..Interval::any() },
            Func::MsBits => {
                let i = p.iv(SAMPLE_BITS);
                if i.single() && i.value() == 32 {
                    p.msbits = if p.msbits == 0 { self.hw.msbits32 } else { p.msbits.min(self.hw.msbits32) };
                }
                return Ok(false);
            }
        };
        match rule.var {
            Some(v) => p.iv_mut(v).refine(&t),
            None => Ok(false),
        }
    }

    /// `snd_pcm_hw_refine` (pcm_native.c:522).
    pub fn refine(&self, p: &mut HwParams) -> Result<(), i64> {
        p.info = 0;
        p.fifo_size = 0;
        if p.rmask & bit(SAMPLE_BITS) != 0 {
            p.msbits = 0;
        }
        if p.rmask & bit(RATE) != 0 {
            p.rate_num = 0;
            p.rate_den = 0;
        }
        for k in ACCESS..=SUBFORMAT {
            if p.masks[k].empty() {
                return Err(EINVAL);
            }
            if p.rmask & bit(k) != 0 && p.masks[k].refine(&self.masks[k])? {
                p.cmask |= bit(k);
            }
        }
        for k in FIRST_INTERVAL..=LAST {
            if p.iv(k).empty {
                return Err(EINVAL);
            }
            if p.rmask & bit(k) != 0 && p.iv_mut(k).refine(&self.iv[k - FIRST_INTERVAL])? {
                p.cmask |= bit(k);
            }
        }
        // constrain_params_by_rules (:347): stamps decide which rules see changed dependencies.
        let mut rstamps = [0u32; RULES.len()];
        let mut vstamps = [0u32; LAST + 1];
        for (k, v) in vstamps.iter_mut().enumerate() {
            *v = (p.rmask & bit(k) != 0) as u32;
        }
        let mut stamp = 2u32;
        loop {
            let mut again = false;
            for (k, rule) in RULES.iter().enumerate() {
                if !rule.deps.iter().any(|&d| vstamps[d] > rstamps[k]) {
                    continue;
                }
                let changed = self.apply(p, rule)?;
                if changed && let Some(v) = rule.var {
                    p.cmask |= bit(v);
                    vstamps[v] = stamp;
                    again = true;
                }
                rstamps[k] = stamp;
                stamp += 1;
            }
            if !again {
                break;
            }
        }
        p.rmask = 0;
        Ok(())
    }

    /// `fixup_unreferenced_params` (pcm_native.c:458); fifo_size stays 0 (`hw.fifo_size` is 0).
    pub fn fixup(&self, p: &mut HwParams) {
        if p.msbits == 0 {
            let i = p.iv(SAMPLE_BITS);
            if i.single() {
                p.msbits = i.value();
            }
        }
        if p.rate_den == 0 {
            let i = p.iv(RATE);
            if i.single() {
                p.rate_num = i.value();
                p.rate_den = 1;
            }
        }
        if p.info == 0 {
            // No mmap (the data ring is the mixer's client ring): MMAP bits off as `hw_support_mmap` false.
            p.info = self.hw.info & !(INFO_FIFO_IN_FRAMES | INFO_DRAIN_TRIGGER | INFO_MMAP | INFO_MMAP_VALID);
        }
    }

    fn first_last(&self, p: &mut HwParams, k: usize, last: bool) -> Result<(), i64> {
        let changed = if k <= SUBFORMAT {
            if last { p.masks[k].refine_last() } else { p.masks[k].refine_first() }
        } else if last {
            p.iv_mut(k).refine_last()?
        } else {
            p.iv_mut(k).refine_first()?
        };
        if changed {
            p.cmask |= bit(k);
            p.rmask |= bit(k);
        }
        if p.rmask != 0 {
            self.refine(p)?;
        }
        // snd_pcm_hw_param_value: the parameter must be single now.
        let single = if k <= SUBFORMAT { p.masks[k].single() } else { p.iv(k).single() };
        if single { Ok(()) } else { Err(EINVAL) }
    }

    /// `snd_pcm_hw_params_choose` (pcm_native.c:636): first access, format, subformat, min channels, rate,
    /// period time, max buffer size, min tick time.
    pub fn choose(&self, p: &mut HwParams) -> Result<(), i64> {
        for k in [ACCESS, FORMAT, SUBFORMAT, CHANNELS, RATE, PERIOD_TIME, BUFFER_SIZE, TICK_TIME] {
            self.first_last(p, k, k == BUFFER_SIZE)?;
        }
        Ok(())
    }

    /// What HW_PARAMS does before the driver sees it: refine everything, choose, fix up.
    pub fn hw_params(&self, p: &mut HwParams) -> Result<(), i64> {
        p.rmask = !0;
        self.refine(p)?;
        self.choose(p)?;
        self.fixup(p);
        Ok(())
    }
}
