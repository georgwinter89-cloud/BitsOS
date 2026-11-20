//! GM-6c: the sofa mode's quick menu (plan/spec/sofa-mode.md 2.4, §7): Läuft gerade with Weiterspielen and Spiel
//! beenden (asked once; then the window's close, and after `END_AFTER` the hung window's Beenden plus SIGTERM to its
//! process group), the volume slider (◀ ▶ by 5 %) and the output chips (kernel.snd_output through settingsd), the
//! tiles WLAN, Gamepad, Bildschirm and Leistung, Zum normalen Desktop, Neu starten and Ausschalten (asked once; then
//! GM-6w's power socket). Everything outside bitscomp is read and asked by sofa_io.rs's thread (B4); this file
//! works on its cached answer. The texts are pure functions (host-tested in sofa_quick_tests.rs).

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use mcpreg::Json;

use crate::pad::Dir;
use crate::sofa::{Front, Way};
use crate::sofa_focus::{Item, Screen};
use crate::sofa_io::{self as io, Battery, Job, Msg, Snap};
use crate::sofa_layout as lay;
use crate::state::App;
use crate::win::Window;

/// Spiel beenden: this long after the window's close its program is ended (sofa-mode.md 2.4).
pub const END_AFTER: Duration = Duration::from_secs(10);
pub const VOLUME_STEP: u8 = 5;

/// A question the sofa mode asks first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    End,
    Reboot,
    Off,
}

impl Confirm {
    pub fn title(self) -> &'static str {
        match self {
            Confirm::End => "Wirklich beenden?",
            Confirm::Reboot => "Neu starten?",
            Confirm::Off => "Ausschalten?",
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Confirm::End => "Nicht gespeicherter Fortschritt geht verloren.",
            Confirm::Reboot | Confirm::Off => "Alle Programme werden beendet.",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Confirm::End => "end",
            Confirm::Reboot => "reboot",
            Confirm::Off => "off",
        }
    }
}

/// The quick menu's state (in `Sofa`).
#[derive(Default)]
pub struct Quick {
    pub io: Snap,
    pub tx: Option<mpsc::Sender<Job>>,
    /// The power socket's path (None: not on BitsOS and no test path).
    pub power: Option<PathBuf>,
    /// A line to show (a refusal, the power request's answer).
    pub note: Option<String>,
    /// Spiel beenden: the window asked to close and when its program is ended.
    pub ending: Option<(Window, Duration)>,
    /// Spiel beenden: the window shown when the question was asked (Ja ends that one, not whatever is in front then).
    pub asked: Option<Window>,
}

/// The chip's words of an output (kernel.snd_output's value).
pub fn output_label(o: &str) -> &'static str {
    match o {
        "hdmi" => "Fernseher (HDMI)",
        "jack" => "Kopfhörer (Klinke)",
        "usb" => "Kopfhörer (USB)",
        _ => "Automatisch",
    }
}

fn output_word(o: &str) -> &'static str {
    match o {
        "hdmi" => "HDMI",
        "jack" => "Klinke",
        "usb" => "USB",
        _ => "",
    }
}

/// The output shown as active: the chosen one (kernel.snd_output), with `auto` the one that plays.
pub fn active_output(setting: Option<&str>, playing: Option<&str>) -> Option<String> {
    match setting {
        Some(s) if s != "auto" && !s.is_empty() => Some(s.to_string()),
        _ => playing.map(str::to_string),
    }
}

/// "30 % · HDMI" (without an output: "30 %"; without a control: "nicht da").
pub fn volume_text(v: Option<u8>, out: Option<&str>) -> String {
    match (v, out.map(output_word).filter(|w| !w.is_empty())) {
        (Some(p), Some(w)) => format!("{p} % · {w}"),
        (Some(p), None) => format!("{p} %"),
        (None, _) => "nicht da".into(),
    }
}

/// The header's word: "Ton 30 %", or "Ton –" while no value is known.
pub fn header_word(v: Option<u8>) -> String {
    v.map_or_else(|| crate::sofa_app::VOLUME_UNKNOWN.to_string(), |p| format!("Ton {p} %"))
}

/// The slider's next value by ◀ ▶ (5 % steps, onto the grid of 5).
pub fn step_volume(v: u8, d: i32) -> u8 {
    let s = VOLUME_STEP as i32;
    let v = v.min(100) as i32;
    let to = if d > 0 { (v / s + 1) * s } else { (v + s - 1) / s * s - s };
    to.clamp(0, 100) as u8
}

/// The WLAN tile: the network's name and state.
pub fn wlan_text(link: Option<&str>, ssid: Option<&str>) -> String {
    let ssid = ssid.map(str::trim).filter(|s| !s.is_empty());
    match (link, ssid) {
        (Some("WLAN"), Some(n)) => format!("{n} · verbunden"),
        (Some("WLAN"), None) => "Verbunden".into(),
        (Some("Kabel"), _) => "Kabel · verbunden".into(),
        (_, Some(n)) => format!("{n} · getrennt"),
        _ => "Kein Netz".into(),
    }
}

/// The Gamepad tile: "Verbunden · 80 %", "Verbunden · Kabel" without a battery, "Verbunden" while the battery
/// has not said its value yet (status Unknown: never "0 %", GM-6p), "Keins verbunden".
pub fn pad_text(pads: usize, b: Option<&Battery>) -> String {
    match (pads, b.and_then(|b| b.capacity)) {
        (0, _) => "Keins verbunden".into(),
        (_, _) if b.is_some_and(|b| b.status == "Unknown") => "Verbunden".into(),
        (_, Some(c)) if b.is_some_and(|b| b.status == "Charging") => format!("Verbunden · lädt · {c} %"),
        (_, Some(c)) => format!("Verbunden · {c} %"),
        (_, None) => "Verbunden · Kabel".into(),
    }
}

/// The Leistung tile from the status bar's blocks: "CPU 41 % · 68 °C".
pub fn power_text(blocks: &[crate::status::Block]) -> String {
    let v = |l: &str| blocks.iter().find(|b| b.label == l).map(|b| b.value.clone());
    let cpu = format!("CPU {}", v("CPU").unwrap_or_else(|| "–".into()));
    match v("Temperatur") {
        Some(t) => format!("{cpu} · {t}"),
        None => cpu,
    }
}

pub fn screen_text(w: i32, h: i32) -> String {
    format!("{w} × {h}")
}

impl App {
    /// The quick menu's thread (main.rs, once); `real`: bitscomp shows BitsOS's screen (the fbdev back end).
    pub fn sofa_io_start(&mut self, handle: &smithay::reexports::calloop::LoopHandle<'static, App>, real: bool) {
        let paths = io::Paths::of(real, |k| std::env::var(k).ok());
        self.sofa.quick.power = paths.power.clone();
        if paths.card.is_none() {
            eprintln!("bitscomp: sofa: no sound card to ask (not on BitsOS): the volume shows \"Ton –\"");
        }
        match io::start(handle, paths, |app: &mut App, m| app.sofa_io_came(m)) {
            Ok(tx) => self.sofa.quick.tx = Some(tx),
            Err(e) => eprintln!("bitscomp: sofa: {e} (no volume, no power request)"),
        }
    }

    pub fn sofa_io(&mut self, job: Job) {
        if let Some(tx) = &self.sofa.quick.tx
            && tx.send(job).is_err()
        {
            eprintln!("bitscomp: sofa: the volume thread is gone");
        }
    }

    fn sofa_io_came(&mut self, m: Msg) {
        match m {
            Msg::Snap(s) => {
                let q = &mut self.sofa.quick;
                if let Some((from, to)) = s.lowered {
                    eprintln!("bitscomp: sofa: volume {from} % lowered to {to} % (sofa.start_volume)");
                }
                if s.why != q.io.why
                    && let Some(w) = &s.why
                {
                    eprintln!("bitscomp: sofa: no volume: {w}");
                }
                if s.volume != q.io.volume
                    && let Some(v) = s.volume
                {
                    eprintln!("bitscomp: sofa: volume {v} %");
                }
                if s != q.io {
                    q.io = s;
                    self.needs_frame = true;
                }
            }
            Msg::Power(text, down) => {
                eprintln!("bitscomp: sofa: power request: {text}");
                self.sofa.quick.note = Some(text.clone());
                self.sofa.set.note = Some(text);
                if down {
                    eprintln!("bitscomp: sofa: init goes down: every program gets SIGTERM from it");
                }
                self.needs_frame = true;
            }
        }
    }

    /// The output chips' (value, label).
    pub fn sofa_outputs(&self) -> Vec<(&'static str, &'static str)> {
        self.sofa.quick.io.outputs.iter().map(|o| (*o, output_label(o))).collect()
    }

    /// The program in front on `Sofa` (Läuft gerade), its window and name.
    pub fn sofa_running(&self) -> Option<(Window, String)> {
        if self.sofa.front != Front::Program {
            return None;
        }
        let w = self.ws.focused().cloned().or_else(|| self.ws.tiles.first().and_then(|t| t.stack.get(t.top).cloned())).or_else(|| self.ws.floating.first().map(|f| f.win.clone()))?;
        let (app_id, title) = crate::ctl::names(&w);
        let name = self.library.entries.iter().find(|e| e.owns(&app_id)).map(|e| e.name.clone()).filter(|n| !n.is_empty());
        Some((w, name.or_else(|| (!title.is_empty()).then(|| title.clone())).unwrap_or(app_id)))
    }

    pub fn sofa_quick_items(&self) -> Vec<Item> {
        let (bw, bh) = self.sofa_frame().board();
        let labels: Vec<&str> = self.sofa_outputs().into_iter().map(|(_, l)| l).collect();
        lay::quick(bw, bh, self.sofa_running().is_some(), &labels, &crate::sofa_draw::measure)
    }

    /// The quick menu came: the focus on Weiterspielen (or the slider), the volume asked again.
    pub fn sofa_quick_opened(&mut self) {
        if self.sofa.quick.ending.is_none() {
            self.sofa.quick.note = None;
        }
        self.sofa.confirm = None;
        self.sofa.focus = Some(if self.sofa_running().is_some() { "q:resume" } else { "q:volume" }.into());
        self.sofa_io(Job::Ask);
    }

    /// ◀ ▶ on the slider; true when the focus is on it (the move is the slider's then).
    pub fn sofa_quick_slide(&mut self, d: Dir) -> bool {
        if !self.sofa.nav.quick || self.sofa.confirm.is_some() || self.sofa.focus.as_deref() != Some("q:volume") || !matches!(d, Dir::Left | Dir::Right) {
            return false;
        }
        let Some(v) = self.sofa.quick.io.volume else {
            self.sofa.quick.note = Some("Kein Lautstärkeregler gefunden.".into());
            self.needs_frame = true;
            return true;
        };
        let to = step_volume(v, if d == Dir::Right { 1 } else { -1 });
        self.sofa_volume(to);
        true
    }

    /// The volume to `pct` (shown at once; the thread writes it and reads it back).
    pub fn sofa_volume(&mut self, pct: u8) {
        if self.sofa.quick.io.volume.is_none() {
            return;
        }
        self.sofa.quick.io.volume = Some(pct);
        self.sofa_io(Job::Volume(pct));
        self.habit_sound(None, Some(pct));
        self.needs_frame = true;
    }

    /// A click on the slider at board x `x`.
    pub fn sofa_quick_slide_to(&mut self, x: f64) {
        let Some(r) = self.sofa_quick_items().into_iter().find(|i| i.id == "q:volume").map(|i| i.rect) else { return };
        let p = ((x - r.x as f64) / r.w.max(1) as f64 * 100.0).round().clamp(0.0, 100.0) as u8;
        self.sofa_volume(p);
    }

    /// A on a quick menu element; false for ids that are not the quick menu's.
    pub fn sofa_quick_activate(&mut self, id: &str) -> bool {
        match id {
            "q:resume" => self.sofa_switch(Way::Home),
            "q:end" => match &self.sofa.quick.ending {
                Some(_) => self.sofa.quick.note = Some("Wird schon beendet …".into()),
                None => {
                    self.sofa.quick.asked = self.sofa_running().map(|(w, _)| w);
                    self.sofa_ask(Confirm::End);
                }
            },
            "q:volume" => {}
            "q:wlan" => self.sofa_quick_settings(2),
            "q:pad" => self.sofa_quick_settings(1),
            "q:screen" => self.sofa_quick_settings(0),
            "q:power" => {}
            "q:desktop" => self.sofa_switch(Way::Desktop),
            "q:reboot" => self.sofa_power_ask(Confirm::Reboot),
            "q:off" => self.sofa_power_ask(Confirm::Off),
            o => match o.strip_prefix("q:out:").and_then(|n| n.parse::<usize>().ok()).and_then(|n| self.sofa_outputs().get(n).copied()) {
                Some((value, _)) => {
                    self.habit_sound(Some(output_label(value)), None);
                    self.sofa_settings_set("kernel.snd_output", Json::str(value));
                }
                None => return false,
            },
        }
        self.needs_frame = true;
        true
    }

    /// A tile opening Einstellungen at a section: over a program in front the program is minimised first (the
    /// start screen's hero brings it back, GM-6b's rule).
    fn sofa_quick_settings(&mut self, section: usize) {
        if let Some((w, _)) = self.sofa_running() {
            let _ = self.minimize(&w, "sofa quick menu: Einstellungen");
        }
        self.sofa.nav.quick = false;
        self.sofa.confirm = None;
        self.sofa.front = Front::Shell;
        self.sofa.set.section = section;
        self.sofa_go(Screen::Einstellungen);
        self.sofa.focus = Some(format!("section:{section}"));
        self.sofa_changed();
    }

    pub fn sofa_ask(&mut self, c: Confirm) {
        eprintln!("bitscomp: sofa: asked \"{} {}\"", c.title(), c.text());
        self.sofa.confirm = Some(c);
        self.sofa.focus = Some("c:no".into());
        self.needs_frame = true;
    }

    /// Neu starten, Ausschalten: asked first when init's socket is there; else the line that it comes later.
    pub fn sofa_power_ask(&mut self, c: Confirm) {
        let op = if c == Confirm::Reboot { "reboot" } else { "poweroff" };
        if !self.sofa.quick.power.as_ref().is_some_and(|p| p.exists()) {
            let t = io::power_later(op).to_string();
            eprintln!("bitscomp: sofa: {op}: {t} (no {})", self.sofa.quick.power.as_ref().map_or("power socket".into(), |p| p.display().to_string()));
            self.sofa.quick.note = Some(t.clone());
            self.sofa.set.note = Some(t);
            self.needs_frame = true;
            return;
        }
        self.sofa_ask(c);
    }

    /// Ja or Nein on the question.
    pub fn sofa_confirm(&mut self, yes: bool) {
        let Some(c) = self.sofa.confirm.take() else { return };
        eprintln!("bitscomp: sofa: \"{}\" answered {}", c.title(), if yes { "Ja" } else { "Nein" });
        let back = match c {
            Confirm::End => "q:end",
            Confirm::Reboot => if self.sofa.nav.quick { "q:reboot" } else { "row:2" },
            Confirm::Off => if self.sofa.nav.quick { "q:off" } else { "row:3" },
        };
        self.sofa.focus = Some(back.into());
        self.needs_frame = true;
        if !yes {
            return;
        }
        match c {
            Confirm::End => self.sofa_end_game(),
            Confirm::Reboot | Confirm::Off => {
                let op = if c == Confirm::Reboot { "reboot" } else { "poweroff" };
                // GM-6w: init needs the statement that real input asked; only a physical press makes it.
                let physical = self.input_origin == crate::input::Origin::Physical;
                eprintln!("bitscomp: sofa: power request {op} (input {})", self.input_origin.name());
                self.sofa_io(Job::Power { op, physical });
            }
        }
    }

    /// Spiel beenden, confirmed: the window asked about gets its close now; `END_AFTER` later its program is ended
    /// if it stays. The quick menu stays open with "… wird beendet" until the window goes (sofa_window_gone closes
    /// it then), so a game that takes its time never looks like a dead button.
    fn sofa_end_game(&mut self) {
        let asked = self.sofa.quick.asked.take().filter(|w| self.all_windows().contains(w));
        let Some((w, name)) = asked.map(|w| {
            let n = self.sofa_running().filter(|(r, _)| *r == w).map_or_else(|| crate::ctl::names(&w).1, |(_, n)| n);
            (w, n)
        }) else {
            eprintln!("bitscomp: sofa: Spiel beenden: the program asked about is gone already");
            return;
        };
        crate::xwm::close(&w);
        eprintln!("bitscomp: sofa: Spiel beenden: {name} ({}) asked to close; ended in {} s if it stays", self.describe(&w), END_AFTER.as_secs());
        self.sofa.quick.ending = Some((w, crate::pacer::now() + END_AFTER));
        self.sofa.quick.note = Some(format!("{name} wird beendet …"));
        self.sofa_changed();
    }

    pub fn sofa_quick_due(&self) -> Option<Duration> {
        self.sofa.quick.ending.as_ref().map(|(_, t)| *t)
    }

    /// The Spiel beenden deadline: the window still there gets the hung window's Beenden (SIGTERM, SIGKILL 2 s
    /// later) and its process group a SIGTERM (when that group is the program's own, `ending::group_ok`).
    pub fn sofa_quick_tick(&mut self, now: Duration) {
        if !self.sofa.quick.ending.as_ref().is_some_and(|(_, t)| now >= *t) {
            return;
        }
        let Some((w, _)) = self.sofa.quick.ending.take() else { return };
        if !self.all_windows().contains(&w) {
            return;
        }
        let pid = self.pid_of(&w);
        // The group and its leader are read before anything is signalled (a reaped pid has none: fails closed).
        let others: Vec<u32> = self.all_windows().iter().filter(|x| **x != w).map(|x| self.pid_of(x)).filter(|p| *p > 1).map(|p| App::group_of(p as u32)).collect();
        let g = u32::try_from(pid).ok().filter(|p| *p > 1).and_then(crate::ending::group_read);
        let leader = g.and_then(crate::ending::leader_comm);
        let target = crate::ending::group_target(g, crate::ending::own_group(), &others, leader.as_deref());
        if let Some((_, name)) = self.sofa_running().filter(|(r, _)| *r == w) {
            self.sofa.quick.note = Some(format!("{name} reagiert nicht: wird beendet …"));
        }
        self.needs_frame = true;
        match self.end_window(&w, "Spiel beenden") {
            Ok(t) => eprintln!("bitscomp: sofa: Spiel beenden: still there after {} s: {t}", END_AFTER.as_secs()),
            Err(e) => eprintln!("bitscomp: sofa: Spiel beenden: {e}"),
        }
        if let Some(g) = target {
            self.prog_asked(g);
        }
        match target {
            Some(g) => match crate::ending::signal_group(g, libc::SIGTERM) {
                Ok(()) => eprintln!("bitscomp: sofa: Spiel beenden: SIGTERM to process group {g}"),
                Err(e) => eprintln!("bitscomp: sofa: Spiel beenden: SIGTERM to process group {g} failed: {e}"),
            },
            None => eprintln!(
                "bitscomp: sofa: Spiel beenden: process group {} ({}) not signalled: unreadable, shared or Steam's",
                g.map_or("?".into(), |g| g.to_string()),
                leader.as_deref().unwrap_or("?")
            ),
        }
    }

    /// The window of Spiel beenden went by itself.
    pub fn sofa_quick_gone(&mut self) {
        if let Some((w, _)) = &self.sofa.quick.ending
            && !self.all_windows().contains(w)
        {
            eprintln!("bitscomp: sofa: Spiel beenden: the program ended");
            self.sofa.quick.ending = None;
            self.sofa.quick.note = None;
        }
    }

    /// What the quick panel draws.
    pub fn sofa_quick_look(&self, clock: &str) -> crate::sofa_quick_draw::QuickLook {
        let (bw, bh) = self.sofa_frame().board();
        let io = &self.sofa.quick.io;
        let active = active_output(self.settings.text("kernel.snd_output"), io.playing.as_deref());
        let link = self.status.blocks.iter().find(|b| b.label == "Netz").map(|b| b.value.as_str());
        let (w, h) = crate::chrome_render::wallpaper::output_px(self);
        crate::sofa_quick_draw::QuickLook {
            frame: self.sofa_frame(),
            bw,
            bh,
            clock: clock.to_string(),
            running: self.sofa_running().map(|(_, n)| n),
            volume: io.volume,
            volume_text: volume_text(io.volume, active.as_deref()),
            outputs: self.sofa_outputs().into_iter().map(|(v, l)| (l.to_string(), active.as_deref() == Some(v))).collect(),
            tiles: vec![
                ("WLAN", wlan_text(link, self.settings.text("net.wifi.ssid"))),
                ("Gamepad", pad_text(self.pads.list.len(), self.sofa_pad_battery())),
                ("Bildschirm", screen_text(w, h)),
                ("Leistung", power_text(&self.status.blocks)),
            ],
            note: self.sofa.quick.note.clone(),
            confirm: self.sofa.confirm,
            items: self.sofa_quick_items(),
            focus: self.sofa.focus.clone(),
        }
    }

    /// The battery of a connected pad (by its phys; another HID device's battery is never the pad's).
    pub fn sofa_pad_battery(&self) -> Option<&Battery> {
        let phys: Vec<&str> = self.pads.list.iter().map(|(_, d)| d.phys.as_str()).collect();
        io::pad_battery(&phys, &self.sofa.quick.io.batteries)
    }

    /// `ctl sofa state`'s words of the quick menu and Einstellungen.
    pub fn sofa_quick_state(&self) -> String {
        let io = &self.sofa.quick.io;
        format!(
            " volume={} battery={} outputs={} confirm={} ending={} section={} rows={} learn={} note={:?}",
            io.volume.map_or("-".into(), |v| v.to_string()),
            match self.sofa_pad_battery() {
                Some(b) => b.capacity.map_or("unknown".into(), |c| c.to_string()),
                None => "kabel".into(),
            },
            if io.outputs.is_empty() { "-".into() } else { io.outputs.join(",") },
            self.sofa.confirm.map_or("-", Confirm::word),
            if self.sofa.quick.ending.is_some() { "yes" } else { "no" },
            self.sofa.set.section,
            self.sofa_set_rows().len(),
            self.sofa.set.learn.as_ref().map_or("-".to_string(), |l| l.got.len().to_string()),
            self.sofa.quick.note.as_deref().or(self.sofa.set.note.as_deref()).unwrap_or(""),
        )
    }
}

#[cfg(test)]
#[path = "sofa_quick_tests.rs"]
mod tests;
