//! PK-L1c: a gamepad's raw evdev events become logical presses (A, B, X, Y, LB, RB, Select, Start, Home and the
//! four directions) for the library overlay now and the sofa mode later (plan/spec/sofa-mode.md section 4).
//!
//! Which raw button is which comes from one of three sources: the standard mapping of a pad that reports Linux's
//! gamepad codes (BTN_SOUTH ..., linux Documentation/input/gamepad.rst), a small table keyed by vendor:product for
//! joystick-class pads (Georg's DragonRise 0079:0006 reports BTN_TRIGGER..BTN_BASE6), or a fallback for unknown
//! joysticks (their first four buttons are A, B, X, Y). A pad without a Home button gets Home from Select+Start
//! held together for `CHORD_MS` (sofa-mode.md section 4). The D-pad (hat or BTN_DPAD_*) and the left stick (dead
//! zone 25 % or the device's flat, a direction past 50 %) move; a held direction repeats after `REPEAT_FIRST_MS`,
//! then every `REPEAT_MS`. Pure: events with millisecond times in, presses out; host-tested in `pad_tests.rs`.

use std::collections::BTreeSet;

use crate::backend::evdev_map::{ABS_X, ABS_Y, EV_ABS, EV_KEY, EV_SYN, SYN_DROPPED, SYN_REPORT};

pub const BTN_MISC: u16 = 0x100;
pub const BTN_JOYSTICK: u16 = 0x120;
pub const BTN_DEAD: u16 = 0x12f;
/// BTN_GAMEPAD, BTN_A: the bottom face button.
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
/// BTN_X in Linux's naming (xpad reports the Xbox X button there).
pub const BTN_NORTH: u16 = 0x133;
/// BTN_Y in Linux's naming.
pub const BTN_WEST: u16 = 0x134;
pub const BTN_TL: u16 = 0x136;
pub const BTN_TR: u16 = 0x137;
pub const BTN_SELECT: u16 = 0x13a;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_THUMBR: u16 = 0x13e;
pub const BTN_DIGI: u16 = 0x140;
pub const BTN_DPAD_UP: u16 = 0x220;
pub const BTN_DPAD_DOWN: u16 = 0x221;
pub const BTN_DPAD_LEFT: u16 = 0x222;
pub const BTN_DPAD_RIGHT: u16 = 0x223;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;
const KEY_MAX: u16 = 0x2ff;
const VENDOR_SONY: u16 = 0x054c;

pub const CHORD_MS: u64 = 500;
pub const REPEAT_FIRST_MS: u64 = 400;
pub const REPEAT_MS: u64 = 120;
/// The stick's least dead zone, of half its range.
pub const DEAD: f64 = 0.25;
/// Past this share of half the range an axis counts as a direction.
pub const PAST: f64 = 0.5;
/// A held direction that does not repeat (it began while bitscomp ignored the pad).
const NEVER: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    A,
    B,
    X,
    Y,
    LB,
    RB,
    Select,
    Start,
    Home,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    Btn(Btn),
    Move(Dir),
}

impl Press {
    pub fn name(self) -> &'static str {
        match self {
            Press::Btn(Btn::A) => "A",
            Press::Btn(Btn::B) => "B",
            Press::Btn(Btn::X) => "X",
            Press::Btn(Btn::Y) => "Y",
            Press::Btn(Btn::LB) => "LB",
            Press::Btn(Btn::RB) => "RB",
            Press::Btn(Btn::Select) => "Select",
            Press::Btn(Btn::Start) => "Start",
            Press::Btn(Btn::Home) => "Home",
            Press::Move(Dir::Left) => "left",
            Press::Move(Dir::Right) => "right",
            Press::Move(Dir::Up) => "up",
            Press::Move(Dir::Down) => "down",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Standard,
    Table,
    Fallback,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Source::Standard => "standard",
            Source::Table => "table",
            Source::Fallback => "fallback",
        }
    }
}

/// An absolute axis as EVIOCGABS says it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Axis {
    pub min: i32,
    pub max: i32,
    pub flat: i32,
}

impl Axis {
    /// -1..=1 around the range's middle.
    fn norm(self, v: i32) -> f64 {
        let half = (self.max as f64 - self.min as f64) / 2.0;
        if half <= 0.0 {
            return 0.0;
        }
        let mid = (self.max as f64 + self.min as f64) / 2.0;
        ((v as f64 - mid) / half).clamp(-1.0, 1.0)
    }

    fn dead(self) -> f64 {
        let half = (self.max as f64 - self.min as f64) / 2.0;
        if half <= 0.0 { DEAD } else { (self.flat.max(0) as f64 / half).max(DEAD) }
    }

    fn mid(self) -> i32 {
        ((self.min as i64 + self.max as i64) / 2) as i32
    }
}

/// Raw button code -> logical button, and where that came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    pub source: Source,
    pub buttons: Vec<(u16, Btn)>,
}

impl Mapping {
    pub fn of(&self, code: u16) -> Option<Btn> {
        self.buttons.iter().find(|(c, _)| *c == code).map(|(_, b)| *b)
    }

    pub fn has(&self, b: Btn) -> bool {
        self.buttons.iter().any(|(_, x)| *x == b)
    }

    /// Home comes from Select+Start held together: the pad has no Home button but both of those.
    pub fn chord(&self) -> bool {
        !self.has(Btn::Home) && self.has(Btn::Select) && self.has(Btn::Start)
    }
}

/// One row of the joystick table: the button numbers are SDL's (`sdl_order`).
struct Row {
    vendor: u16,
    product: u16,
    buttons: &'static [(usize, Btn)],
}

/// DragonRise Inc. Generic USB Joystick 0079:0006: SDL's community mapping for that id
/// (gabomdq/SDL_GameControllerDB, zlib licence; plan/notes/wy-PK-L1c.md): a:b2 b:b1 x:b3 y:b0 leftshoulder:b4
/// rightshoulder:b5 back:b8 start:b9, the hat as the D-pad.
const TABLE: &[Row] = &[Row {
    vendor: 0x0079,
    product: 0x0006,
    buttons: &[(2, Btn::A), (1, Btn::B), (3, Btn::X), (0, Btn::Y), (4, Btn::LB), (5, Btn::RB), (8, Btn::Select), (9, Btn::Start)],
}];

/// SDL's button numbers on Linux (src/joystick/linux/SDL_sysjoystick.c): the codes from BTN_JOYSTICK up to
/// KEY_MAX, then BTN_MISC up to BTN_JOYSTICK, each in code order; `keys` are the device's EV_KEY codes.
pub fn sdl_order(keys: &[u16]) -> Vec<u16> {
    let mut k: Vec<u16> = keys.to_vec();
    k.sort_unstable();
    k.dedup();
    let hi = k.iter().copied().filter(|c| (BTN_JOYSTICK..KEY_MAX).contains(c));
    let lo = k.iter().copied().filter(|c| (BTN_MISC..BTN_JOYSTICK).contains(c));
    hi.chain(lo).collect()
}

/// The mapping of a pad with vendor:product and EV_KEY codes `keys`.
pub fn mapping(vendor: u16, product: u16, keys: &[u16]) -> Mapping {
    if keys.contains(&BTN_SOUTH) {
        // SDL's rule: Sony pads report the left face button as BTN_WEST and the top one as BTN_NORTH; the others
        // (xpad and most) report X as BTN_NORTH ("BTN_X") and Y as BTN_WEST ("BTN_Y").
        let (x, y) = if vendor == VENDOR_SONY { (BTN_WEST, BTN_NORTH) } else { (BTN_NORTH, BTN_WEST) };
        let all = [
            (BTN_SOUTH, Btn::A),
            (BTN_EAST, Btn::B),
            (x, Btn::X),
            (y, Btn::Y),
            (BTN_TL, Btn::LB),
            (BTN_TR, Btn::RB),
            (BTN_SELECT, Btn::Select),
            (BTN_START, Btn::Start),
            (BTN_MODE, Btn::Home),
        ];
        let buttons = all.into_iter().filter(|(c, _)| keys.contains(c)).collect();
        return Mapping { source: Source::Standard, buttons };
    }
    let order = sdl_order(keys);
    if let Some(row) = TABLE.iter().find(|r| r.vendor == vendor && r.product == product) {
        let buttons = row.buttons.iter().filter_map(|&(i, b)| order.get(i).map(|&c| (c, b))).collect();
        return Mapping { source: Source::Table, buttons };
    }
    let mut buttons: Vec<(u16, Btn)> = order.iter().zip([Btn::A, Btn::B, Btn::X, Btn::Y]).map(|(&c, b)| (c, b)).collect();
    // A guess so that an unknown pad still has Home (D9): with ten buttons or more, SDL's buttons 8 and 9 are
    // Select and Start, as on the DragonRise row above and most generic USB pads.
    if let (Some(&sel), Some(&start)) = (order.get(8), order.get(9)) {
        buttons.extend([(sel, Btn::Select), (start, Btn::Start)]);
    }
    Mapping { source: Source::Fallback, buttons }
}

/// One pad's state between events.
pub struct Pad {
    pub map: Mapping,
    x: Option<Axis>,
    y: Option<Axis>,
    stick: (i32, i32),
    hat: (i32, i32),
    /// BTN_DPAD_LEFT, RIGHT, UP, DOWN.
    dpad: [bool; 4],
    held: BTreeSet<u16>,
    stick_dir: Option<Dir>,
    /// The direction held and when it repeats next.
    dir: Option<(Dir, u64)>,
    /// Select+Start: since when both are held, and whether this hold gave its Home.
    chord: Option<(u64, bool)>,
    frame: Vec<Press>,
    dropping: bool,
}

impl Pad {
    /// `x`/`y`: the left stick's axes (None: the device has no such axis).
    pub fn new(map: Mapping, x: Option<Axis>, y: Option<Axis>) -> Pad {
        let stick = (x.map_or(0, Axis::mid), y.map_or(0, Axis::mid));
        Pad { map, x, y, stick, hat: (0, 0), dpad: [false; 4], held: BTreeSet::new(), stick_dir: None, dir: None, chord: None, frame: Vec::new(), dropping: false }
    }

    /// The axes' values when the pad was opened (EVIOCGABS `value`): a stick or hat already deflected then is
    /// not taken as resting.
    pub fn seed(&mut self, x: Option<i32>, y: Option<i32>, hat: (i32, i32)) {
        if self.x.is_some() && let Some(v) = x {
            self.stick.0 = v;
        }
        if self.y.is_some() && let Some(v) = y {
            self.stick.1 = v;
        }
        self.hat = (hat.0.signum(), hat.1.signum());
    }

    /// One raw event at `now` ms; button presses and directions come out at its SYN_REPORT.
    pub fn feed(&mut self, ty: u16, code: u16, value: i32, now: u64, out: &mut Vec<Press>) {
        if self.dropping {
            if ty == EV_SYN && code == SYN_REPORT {
                self.dropping = false;
            }
            return;
        }
        match (ty, code) {
            (EV_SYN, SYN_REPORT) => self.report(now, out),
            (EV_SYN, SYN_DROPPED) => {
                // Unknown what was lost: everything is let go (no direction keeps repeating).
                self.reset();
                self.dropping = true;
            }
            (EV_KEY, c) if (BTN_DPAD_UP..=BTN_DPAD_RIGHT).contains(&c) && (value == 0 || value == 1) => {
                let i = match c {
                    BTN_DPAD_LEFT => 0,
                    BTN_DPAD_RIGHT => 1,
                    BTN_DPAD_UP => 2,
                    BTN_DPAD_DOWN => 3,
                    _ => return,
                };
                self.dpad[i] = value == 1;
            }
            // value 2 is the kernel's soft repeat: the repeat is ours.
            (EV_KEY, c) if value == 1 => {
                if self.held.insert(c)
                    && let Some(b) = self.map.of(c)
                {
                    self.frame.push(Press::Btn(b));
                }
            }
            (EV_KEY, c) if value == 0 => {
                self.held.remove(&c);
            }
            (EV_ABS, ABS_X) => self.stick.0 = value,
            (EV_ABS, ABS_Y) => self.stick.1 = value,
            (EV_ABS, ABS_HAT0X) => self.hat.0 = value.signum(),
            (EV_ABS, ABS_HAT0Y) => self.hat.1 = value.signum(),
            _ => {}
        }
    }

    fn report(&mut self, now: u64, out: &mut Vec<Press>) {
        out.append(&mut self.frame);
        self.stick_dir = self.stick_now();
        let want = self.dpad_now().or(self.stick_dir);
        if want != self.dir.map(|(d, _)| d) {
            self.dir = want.map(|d| (d, now + REPEAT_FIRST_MS));
            if let Some(d) = want {
                out.push(Press::Move(d));
            }
        }
        let both = self.map.chord() && self.held_btn(Btn::Select) && self.held_btn(Btn::Start);
        match (both, self.chord) {
            (true, None) => self.chord = Some((now, false)),
            (false, Some(_)) => self.chord = None,
            _ => {}
        }
        self.tick(now, out);
    }

    fn held_btn(&self, b: Btn) -> bool {
        self.held.iter().any(|&c| self.map.of(c) == Some(b))
    }

    fn dpad_now(&self) -> Option<Dir> {
        let (l, r, u, d) = (self.dpad[0] || self.hat.0 < 0, self.dpad[1] || self.hat.0 > 0, self.dpad[2] || self.hat.1 < 0, self.dpad[3] || self.hat.1 > 0);
        // A diagonal on the hat counts as its horizontal half.
        match (l, r, u, d) {
            (true, false, ..) => Some(Dir::Left),
            (false, true, ..) => Some(Dir::Right),
            (_, _, true, false) => Some(Dir::Up),
            (_, _, false, true) => Some(Dir::Down),
            _ => None,
        }
    }

    /// The stick's direction: the stronger axis once it passes 50 %; a direction held stays while its axis is
    /// outside the dead zone the same way (no flicker at 50 %) and the other axis does not take over.
    fn stick_now(&self) -> Option<Dir> {
        let nx = self.x.map_or(0.0, |a| a.norm(self.stick.0));
        let ny = self.y.map_or(0.0, |a| a.norm(self.stick.1));
        let (ax, ay) = (nx.abs(), ny.abs());
        let of = |horizontal: bool| match (horizontal, (if horizontal { nx } else { ny }) < 0.0) {
            (true, true) => Dir::Left,
            (true, false) => Dir::Right,
            (false, true) => Dir::Up,
            (false, false) => Dir::Down,
        };
        if ax.max(ay) > PAST {
            return Some(of(ax >= ay));
        }
        let keep = match self.stick_dir {
            Some(Dir::Left) => nx < 0.0 && ax > self.x.map_or(DEAD, Axis::dead),
            Some(Dir::Right) => nx > 0.0 && ax > self.x.map_or(DEAD, Axis::dead),
            Some(Dir::Up) => ny < 0.0 && ay > self.y.map_or(DEAD, Axis::dead),
            Some(Dir::Down) => ny > 0.0 && ay > self.y.map_or(DEAD, Axis::dead),
            None => false,
        };
        if keep { self.stick_dir } else { None }
    }

    /// The repeat of the held direction and the Select+Start Home, when due; a late tick repeats once, not in a burst.
    pub fn tick(&mut self, now: u64, out: &mut Vec<Press>) {
        if let Some((d, next)) = self.dir
            && now >= next
        {
            out.push(Press::Move(d));
            let n = next.saturating_add(REPEAT_MS);
            self.dir = Some((d, if n <= now { now.saturating_add(REPEAT_MS) } else { n }));
        }
        if let Some((since, false)) = self.chord
            && now >= since + CHORD_MS
        {
            out.push(Press::Btn(Btn::Home));
            self.chord = Some((since, true));
        }
    }

    /// When `tick` has something to do next.
    pub fn due(&self) -> Option<u64> {
        let rep = self.dir.map(|(_, t)| t).filter(|&t| t != NEVER);
        rep.into_iter().chain(self.chord_due()).min()
    }

    /// When the Select+Start hold gives its Home (the only timer that matters while the library is closed).
    pub fn chord_due(&self) -> Option<u64> {
        match self.chord {
            Some((since, false)) => Some(since + CHORD_MS),
            _ => None,
        }
    }

    /// Nobody listens to the directions (the library is closed): the held one stops repeating, so a game's held
    /// stick does not wake bitscomp every 120 ms; a new direction arms the repeat again.
    pub fn quiet(&mut self) {
        if let Some((d, _)) = self.dir {
            self.dir = Some((d, NEVER));
        }
    }

    /// Everything let go (the pad went away, events were dropped): no repeat, no chord, nothing comes out.
    pub fn reset(&mut self) {
        self.held.clear();
        self.dpad = [false; 4];
        self.hat = (0, 0);
        self.stick = (self.x.map_or(0, Axis::mid), self.y.map_or(0, Axis::mid));
        self.stick_dir = None;
        self.dir = None;
        self.chord = None;
        self.frame.clear();
    }
}

#[cfg(test)]
#[path = "pad_tests.rs"]
mod tests;
