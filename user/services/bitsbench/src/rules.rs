//! When the nightly set may run (Georg 13.11.: only while switched on, the default since 18.11., in the night window,
//! on mains power, with nobody using the machine: no input for 30 minutes, no full-screen program, no game, nothing
//! playing sound, no download) and when a run must stop. Pure: the facts come from `probe` and bitscomp.

use crate::time::Local;

/// No input for this long counts as nobody using the machine.
pub const IDLE_S: u64 = 30 * 60;
/// More received than this per second over the last minute counts as a download in progress.
pub const DOWNLOAD_BPS: f64 = 256.0 * 1024.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub on: bool,
    /// The window's first and last hour (local time); `to` is where no new run starts any more.
    pub from_h: u32,
    pub to_h: u32,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings { on: true, from_h: 2, to_h: 5 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Power {
    Mains,
    Battery,
    /// No supply says anything (a desktop machine without ACPI AC): counts as mains.
    Unknown,
}

/// What the desktop says (bitscomp `ctl idle`); None when it does not answer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desk {
    pub idle_s: u64,
    /// Physical input events since bitscomp started (a run ends when this moves).
    pub inputs: u64,
    pub fullscreen: bool,
    pub games: u32,
    pub offset_min: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Facts {
    pub now_s: i64,
    pub desk: Option<Desk>,
    pub power: Power,
    /// Bytes per second received over the last minute (None: not measured yet).
    pub rx_bps: Option<f64>,
    /// The start of the last run that finished (UTC seconds).
    pub last_done_s: Option<i64>,
    /// Something plays sound (the music player, or any program with sound open at the kernel's mixer): someone
    /// falling asleep to a playlist or a video uses the machine without touching it.
    pub sound: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Off,
    NoDesktop,
    Outside,
    DoneTonight,
    InUse,
    FullScreen,
    Game,
    Sound,
    Download,
    Battery,
}

impl Why {
    pub fn word(self) -> &'static str {
        match self {
            Why::Off => "off",
            Why::NoDesktop => "no_desktop",
            Why::Outside => "outside_window",
            Why::DoneTonight => "done_tonight",
            Why::InUse => "in_use",
            Why::FullScreen => "fullscreen",
            Why::Game => "game",
            Why::Sound => "sound",
            Why::Download => "download",
            Why::Battery => "battery",
        }
    }
}

/// Whether the local minute `min` lies in the window: from `from_h`:00 up to `to_h`:00, over midnight when
/// `to_h` < `from_h`. Equal hours make an empty window (never), not a whole day: a night run all day long is
/// not what the switch promises.
pub fn in_window(min: u32, from_h: u32, to_h: u32) -> bool {
    let (a, b) = (from_h.min(23) * 60, to_h.min(23) * 60);
    if a == b {
        return false;
    }
    if a < b { (a..b).contains(&min) } else { min >= a || min < b }
}

/// Minutes since the window last opened (its start may lie before midnight).
pub fn since_open(min: u32, from_h: u32) -> u32 {
    (min + 1440 - from_h.min(23) * 60) % 1440
}

pub fn decide(s: &Settings, f: &Facts) -> Result<(), Why> {
    if !s.on {
        return Err(Why::Off);
    }
    let d = f.desk.ok_or(Why::NoDesktop)?;
    let local = Local::of(f.now_s, d.offset_min);
    if !in_window(local.minute, s.from_h, s.to_h) {
        return Err(Why::Outside);
    }
    let opened = f.now_s - i64::from(since_open(local.minute, s.from_h)) * 60 - f.now_s.rem_euclid(60);
    if f.last_done_s.is_some_and(|t| t >= opened) {
        return Err(Why::DoneTonight);
    }
    if f.power == Power::Battery {
        return Err(Why::Battery);
    }
    if d.idle_s < IDLE_S {
        return Err(Why::InUse);
    }
    if d.fullscreen {
        return Err(Why::FullScreen);
    }
    if d.games > 0 {
        return Err(Why::Game);
    }
    if f.sound {
        return Err(Why::Sound);
    }
    if f.rx_bps.is_none_or(|r| r > DOWNLOAD_BPS) {
        return Err(Why::Download);
    }
    Ok(())
}

/// The power state from the supplies under /sys/class/power_supply: (type, online, scope, status) each. A mains
/// supply decides when there is one; else a system battery that discharges means battery. Device batteries
/// (a gamepad's, a mouse's: scope "Device") never count.
pub fn power_of(supplies: &[(String, Option<bool>, String, String)]) -> Power {
    let mains: Vec<_> = supplies.iter().filter(|s| s.0 == "Mains").collect();
    if !mains.is_empty() {
        return if mains.iter().any(|s| s.1 == Some(true)) { Power::Mains } else { Power::Battery };
    }
    let system = supplies.iter().filter(|s| s.0 == "Battery" && s.2 != "Device");
    if system.clone().any(|s| s.3 == "Discharging") {
        return Power::Battery;
    }
    if system.count() > 0 { Power::Mains } else { Power::Unknown }
}

/// A run in progress stops when a physical input came after its start: bitscomp's count moved (a count that went
/// back means bitscomp started anew, which a run does not survive either).
pub fn input_came(at_start: &Desk, now: &Desk) -> bool {
    now.inputs != at_start.inputs
}
