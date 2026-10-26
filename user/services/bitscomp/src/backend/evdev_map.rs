//! Pure part of the evdev reader: what a device is (from its EVIOCGBIT/EVIOCGABS answers) and how its raw
//! `input_event`s become bitscomp's `InputEvent`s (linux-6.6/include/uapi/linux/input-event-codes.h,
//! Documentation/input/event-codes.rst). Host-tested in `evdev_tests.rs`; the ioctls and reads are in `evdev`.

use std::collections::BTreeSet;

pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_REL: u16 = 2;
pub const EV_ABS: u16 = 3;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;
pub const REL_X: u16 = 0;
pub const REL_Y: u16 = 1;
pub const REL_HWHEEL: u16 = 6;
pub const REL_WHEEL: u16 = 8;
pub const REL_WHEEL_HI_RES: u16 = 11;
pub const REL_HWHEEL_HI_RES: u16 = 12;
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const KEY_A: u16 = 30;
pub const KEY_Z: u16 = 44;
pub const BTN_MISC: u16 = 0x100;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_TASK: u16 = 0x117;
pub const BTN_TOUCH: u16 = 0x14a;
/// First key code after the button block (KEY_OK); the block 0x100..0x160 is buttons of all kinds.
pub const KEY_OK: u16 = 0x160;
pub const KEY_MAX: u16 = 0x2ff;

/// An absolute axis's range (EVIOCGABS).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Range {
    pub min: i32,
    pub max: i32,
}

/// What bitscomp uses of a device, from its capability bits.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    pub keyboard: bool,
    pub buttons: bool,
    pub rel_xy: bool,
    pub wheel: bool,
    pub wheel_hi: bool,
    pub hwheel: bool,
    pub hwheel_hi: bool,
    pub abs: Option<(Range, Range)>,
}

fn bit(bits: &[u8], n: u16) -> bool {
    bits.get(n as usize / 8).is_some_and(|b| b & (1 << (n % 8)) != 0)
}

impl Caps {
    /// From the EV_KEY, EV_REL and EV_ABS bitmaps (bytes as EVIOCGBIT fills them) and the X/Y ranges.
    pub fn from_bits(keys: &[u8], rel: &[u8], abs_bits: &[u8], abs: Option<(Range, Range)>) -> Caps {
        let has_abs = bit(abs_bits, ABS_X) && bit(abs_bits, ABS_Y);
        Caps {
            keyboard: bit(keys, KEY_A) && bit(keys, KEY_Z),
            buttons: (BTN_LEFT..=BTN_TASK).any(|b| bit(keys, b)),
            rel_xy: bit(rel, REL_X) && bit(rel, REL_Y),
            wheel: bit(rel, REL_WHEEL),
            wheel_hi: bit(rel, REL_WHEEL_HI_RES),
            hwheel: bit(rel, REL_HWHEEL),
            hwheel_hi: bit(rel, REL_HWHEEL_HI_RES),
            // A joystick or pad has X/Y too but no left button or touch: not a pointer.
            abs: if has_abs && (bit(keys, BTN_LEFT) || bit(keys, BTN_TOUCH)) { abs } else { None },
        }
    }

    pub fn pointer(&self) -> bool {
        self.rel_xy || self.abs.is_some()
    }

    /// Worth opening: a keyboard or a pointer (pads, sensors, power buttons are left to others).
    pub fn wanted(&self) -> bool {
        self.keyboard || self.pointer()
    }
}

/// One device's events after a SYN_REPORT, in output-independent form.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Key { code: u32, pressed: bool },
    Button { code: u32, pressed: bool },
    /// Relative counts, accelerated by `accel` in the back end.
    Rel { dx: f64, dy: f64 },
    /// Position as a fraction 0..=1 of the axis range.
    Abs { fx: f64, fy: f64 },
    /// Wheel steps, positive = down (Wayland's direction); fractional with a high-resolution wheel.
    Wheel { steps: f64 },
    /// Horizontal steps, positive = right.
    HWheel { steps: f64 },
}

/// The per-device state between two SYN_REPORTs.
#[derive(Default)]
pub struct Decoder {
    pub caps: Caps,
    dx: i32,
    dy: i32,
    ax: Option<i32>,
    ay: Option<i32>,
    last_abs: (i32, i32),
    wheel: i32,
    hwheel: i32,
    keys: Vec<Out>,
    /// After SYN_DROPPED everything up to the next SYN_REPORT is dropped (event-codes.rst "SYN_DROPPED").
    dropping: bool,
    /// Keys and buttons this device holds down (released when it goes away or events were dropped).
    pub down: BTreeSet<(bool, u32)>,
}

impl Decoder {
    pub fn new(caps: Caps) -> Decoder {
        let mid = caps.abs.map_or((0, 0), |(x, y)| ((x.min + x.max) / 2, (y.min + y.max) / 2));
        Decoder { caps, last_abs: mid, ..Decoder::default() }
    }

    /// One raw event; at SYN_REPORT the frame's events are appended to `out`.
    pub fn feed(&mut self, ty: u16, code: u16, value: i32, out: &mut Vec<Out>) {
        if self.dropping {
            if ty == EV_SYN && code == SYN_REPORT {
                self.dropping = false;
            }
            return;
        }
        match (ty, code) {
            (EV_SYN, SYN_REPORT) => self.report(out),
            (EV_SYN, SYN_DROPPED) => {
                // Unknown what was lost: forget the frame and let go of everything held (no stuck key).
                self.clear_frame();
                self.release_all(out);
                self.dropping = true;
            }
            // value 2 is the kernel's soft repeat: Wayland clients repeat themselves from repeat_info.
            (EV_KEY, c) if value == 0 || value == 1 => {
                let pressed = value == 1;
                let c32 = c as u32;
                let ev = if (BTN_LEFT..=BTN_TASK).contains(&c) {
                    Out::Button { code: c32, pressed }
                } else if c < BTN_MISC || (KEY_OK..=KEY_MAX).contains(&c) {
                    Out::Key { code: c32, pressed }
                } else {
                    return;
                };
                let is_key = matches!(ev, Out::Key { .. });
                if pressed {
                    if !self.down.insert((is_key, c32)) {
                        return;
                    }
                } else if !self.down.remove(&(is_key, c32)) {
                    return;
                }
                self.keys.push(ev);
            }
            (EV_REL, REL_X) => self.dx = self.dx.saturating_add(value),
            (EV_REL, REL_Y) => self.dy = self.dy.saturating_add(value),
            // With a high-resolution wheel the kernel sends both; the 1/120 one is used and the coarse one dropped.
            (EV_REL, REL_WHEEL_HI_RES) => self.wheel = self.wheel.saturating_add(value),
            (EV_REL, REL_WHEEL) if !self.caps.wheel_hi => self.wheel = self.wheel.saturating_add(value.saturating_mul(120)),
            (EV_REL, REL_HWHEEL_HI_RES) => self.hwheel = self.hwheel.saturating_add(value),
            (EV_REL, REL_HWHEEL) if !self.caps.hwheel_hi => self.hwheel = self.hwheel.saturating_add(value.saturating_mul(120)),
            (EV_ABS, ABS_X) => self.ax = Some(value),
            (EV_ABS, ABS_Y) => self.ay = Some(value),
            _ => {}
        }
    }

    fn clear_frame(&mut self) {
        self.dx = 0;
        self.dy = 0;
        self.ax = None;
        self.ay = None;
        self.wheel = 0;
        self.hwheel = 0;
        self.keys.clear();
    }

    fn report(&mut self, out: &mut Vec<Out>) {
        if (self.ax.is_some() || self.ay.is_some())
            && let Some((rx, ry)) = self.caps.abs
        {
            let x = self.ax.unwrap_or(self.last_abs.0);
            let y = self.ay.unwrap_or(self.last_abs.1);
            self.last_abs = (x, y);
            out.push(Out::Abs { fx: frac(x, rx), fy: frac(y, ry) });
        }
        if self.dx != 0 || self.dy != 0 {
            out.push(Out::Rel { dx: self.dx as f64, dy: self.dy as f64 });
        }
        // Buttons after the motion of the same frame: a click lands where the pointer went.
        out.append(&mut self.keys);
        if self.wheel != 0 {
            out.push(Out::Wheel { steps: -(self.wheel as f64) / 120.0 });
        }
        if self.hwheel != 0 {
            out.push(Out::HWheel { steps: self.hwheel as f64 / 120.0 });
        }
        self.clear_frame();
    }

    /// Releases of everything held (the device went away, or events were dropped).
    pub fn release_all(&mut self, out: &mut Vec<Out>) {
        for (is_key, code) in std::mem::take(&mut self.down) {
            out.push(if is_key { Out::Key { code, pressed: false } } else { Out::Button { code, pressed: false } });
        }
    }
}

fn frac(v: i32, r: Range) -> f64 {
    if r.max <= r.min {
        return 0.0;
    }
    ((v as f64 - r.min as f64) / (r.max as f64 - r.min as f64)).clamp(0.0, 1.0)
}

/// Pointer acceleration, simple and fixed (a settings key later): counts per report up to `SLOW` move 1:1 (exact
/// small moves), above it the factor grows linearly by `GAIN` per count up to `MAX`. A 1000 Hz mouse moved fast
/// (20 counts per report) gets about 2x; at 1440p a slow hand still reaches every pixel.
pub fn accel(dx: f64, dy: f64) -> (f64, f64) {
    const SLOW: f64 = 3.0;
    const GAIN: f64 = 0.06;
    const MAX: f64 = 2.5;
    let speed = dx.hypot(dy);
    let f = if speed <= SLOW { 1.0 } else { (1.0 + (speed - SLOW) * GAIN).min(MAX) };
    (dx * f, dy * f)
}

/// The new pointer position in output pixels after a relative move, kept on the output.
pub fn moved(at: (f64, f64), d: (f64, f64), size: (i32, i32)) -> (f64, f64) {
    let (a, b) = accel(d.0, d.1);
    ((at.0 + a).clamp(0.0, (size.0 - 1).max(0) as f64), (at.1 + b).clamp(0.0, (size.1 - 1).max(0) as f64))
}

/// An absolute position (fractions of the axis ranges) in output pixels.
pub fn placed(fx: f64, fy: f64, size: (i32, i32)) -> (f64, f64) {
    (fx * (size.0 - 1).max(0) as f64, fy * (size.1 - 1).max(0) as f64)
}
