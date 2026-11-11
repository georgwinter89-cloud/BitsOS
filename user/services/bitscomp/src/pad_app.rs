//! PK-L1c: the pads bitscomp reads (`backend::evdev` opens them without a grab) and where their presses go. A pad
//! acts only on bitscomp's own surfaces that understand it: Home (or Select+Start held 500 ms) toggles the library
//! from anywhere, fullscreen programs too (the program still gets the button: nothing is grabbed); while the library
//! is open the D-pad/stick move the focus, A starts, B closes, X opens the circle menu on the focused tile, Y
//! focuses the search field (the on-screen keyboard comes with GM-6). Everything else is ignored by bitscomp. A pad
//! never answers the installer's question, a grant or the full-access dialog: its B refuses as injected input
//! may, every other press there is ignored (MC-9b). `ctl pad` lists the pads; `ctl phys pad-event` is the headless
//! test path into the same state machine.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use smithay::input::keyboard::Keysym;

use crate::input::Origin;
use crate::library_view::Move;
use crate::pad::{Axis, Btn, Dir, Pad, Press, Source};
use crate::state::App;

static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct PadDev {
    pub label: String,
    pub name: String,
    pub id: (u16, u16),
    pub origin: Origin,
    pub pad: Pad,
    /// The last logical press and when (ms of bitscomp's clock), for `ctl pad`.
    pub last: Option<(&'static str, u64)>,
}

impl PadDev {
    pub fn new(label: &str, name: &str, id: (u16, u16), keys: &[u16], stick: (Option<Axis>, Option<Axis>), origin: Origin) -> PadDev {
        let map = crate::pad::mapping(id.0, id.1, keys);
        PadDev { label: label.into(), name: name.into(), id, origin, pad: Pad::new(map, stick.0, stick.1), last: None }
    }

    fn home(&self) -> &'static str {
        let m = &self.pad.map;
        if m.has(Btn::Home) {
            "button"
        } else if m.chord() {
            "Select+Start 500 ms"
        } else {
            "none"
        }
    }
}

#[derive(Default)]
pub struct Pads {
    pub list: Vec<(u64, PadDev)>,
    /// The pads whose press on the shown dialog was already noted (one line per dialog and pad, not one per press:
    /// a game's stick would fill the installer log); cleared when no dialog is shown.
    noted: Vec<u64>,
}

fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

/// Headless test pads for `ctl phys pad-event plug KIND` (their bitmaps as the kernel's fakes report them).
fn test_pad(kind: &str) -> Option<PadDev> {
    let label = format!("test:{kind}");
    let byte = Axis { min: 0, max: 255, flat: 15 };
    Some(match kind {
        "dragonrise" => PadDev::new(&label, "DragonRise Inc.   Generic   USB  Joystick  ", (0x0079, 0x0006), &(0x120..=0x12b).collect::<Vec<_>>(), (Some(byte), Some(byte)), Origin::Physical),
        "xbox" => {
            let keys = [0x130, 0x131, 0x133, 0x134, 0x136, 0x137, 0x13a, 0x13b, 0x13c, 0x13d, 0x13e];
            let a = Axis { min: -32768, max: 32767, flat: 128 };
            PadDev::new(&label, "Microsoft X-Box 360 pad", (0x045e, 0x028e), &keys, (Some(a), Some(a)), Origin::Physical)
        }
        "stick2" => {
            let a = Axis { min: -127, max: 127, flat: 0 };
            PadDev::new(&label, "Two-button stick", (0x1234, 0x5678), &[0x120, 0x121], (Some(a), Some(a)), Origin::Physical)
        }
        _ => return None,
    })
}

fn num(w: Option<&&str>) -> Result<i64, String> {
    let w = w.ok_or("missing number")?;
    let r = if let Some(h) = w.strip_prefix("0x") { i64::from_str_radix(h, 16) } else { w.parse::<i64>() };
    r.map_err(|e| format!("{w}: {e}"))
}

/// `pad` and `pad-event ...`; None for other commands.
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    match words.first().copied() {
        Some("pad") => Some(Ok(app.pad_list())),
        Some("pad-event") => Some(app.pad_event_ctl(&words[1..])),
        _ => None,
    }
}

impl App {
    pub fn pad_plug(&mut self, id: u64, d: PadDev) {
        let src = d.pad.map.source;
        eprintln!(
            "bitscomp: input: pad {} \"{}\" {:04x}:{:04x}: mapping {}, Home {}",
            d.label,
            d.name.trim(),
            d.id.0,
            d.id.1,
            src.name(),
            d.home()
        );
        if src == Source::Fallback {
            let home = if d.pad.map.chord() { "buttons 9 and 10 guessed as Select and Start" } else { "no Home on this pad (fewer than ten buttons)" };
            eprintln!("bitscomp: input: pad {}: unknown pad, its first four buttons are A, B, X, Y (fallback); {home}", d.label);
        }
        self.pads.list.push((id, d));
    }

    /// The pad went away: its held buttons and its repeat are forgotten (nothing comes out of it any more).
    pub fn pad_gone(&mut self, id: u64) {
        if let Some(i) = self.pads.list.iter().position(|(k, _)| *k == id) {
            let (_, d) = self.pads.list.remove(i);
            eprintln!("bitscomp: input: pad {} went away (let go, its repeat stopped)", d.label);
        }
    }

    pub fn pad_feed(&mut self, id: u64, evs: &[(u16, u16, i32)]) {
        let now = ms(crate::pacer::now());
        let Some((_, d)) = self.pads.list.iter_mut().find(|(k, _)| *k == id) else { return };
        let mut out = Vec::new();
        for &(ty, code, value) in evs {
            d.pad.feed(ty, code, value, now, &mut out);
        }
        for p in out {
            self.pad_press(id, p, now);
        }
    }

    /// Repeats wake the loop only while the library listens; the Select+Start hold always (Home works anywhere).
    pub fn pad_due(&self) -> Option<Duration> {
        let open = self.lib.open;
        self.pads.list.iter().filter_map(|(_, d)| if open { d.pad.due() } else { d.pad.chord_due() }).min().map(Duration::from_millis)
    }

    pub fn pad_tick(&mut self, now: Duration) {
        let now = ms(now);
        let open = self.lib.open;
        if !self.install.shown() {
            self.pads.noted.clear();
        }
        let mut due = Vec::new();
        for (id, d) in &mut self.pads.list {
            if !open {
                d.pad.quiet();
            }
            let mut out = Vec::new();
            d.pad.tick(now, &mut out);
            due.extend(out.into_iter().map(|p| (*id, p)));
        }
        for (id, p) in due {
            self.pad_press(id, p, now);
        }
    }

    /// One logical press; `input_origin` is the pad's while it acts and what it was afterwards (no stale origin
    /// for a later path that reads it outside an input event).
    fn pad_press(&mut self, id: u64, p: Press, now: u64) {
        let Some((_, d)) = self.pads.list.iter_mut().find(|(k, _)| *k == id) else { return };
        d.last = Some((p.name(), now));
        let origin = d.origin;
        if matches!(p, Press::Btn(_)) {
            self.lib.alone.other();
        }
        let keep = self.input_origin;
        self.pad_route(id, p, origin);
        self.input_origin = keep;
    }

    fn pad_route(&mut self, id: u64, p: Press, origin: Origin) {
        // MC-9b: on the installer's question, a grant or the full-access dialog a pad may only refuse. The dialog
        // is the topmost layer and takes every key while shown (`install_takes_keys`), over a fullscreen game too:
        // a B pressed then refuses a question that is on the screen, the safe side.
        if self.install.shown() {
            if p == Press::Btn(Btn::B) {
                self.install_note("pad B: refused as injected input (a pad never says yes)".into());
                self.input_origin = Origin::Injected;
                self.install_key(Keysym::Escape);
            } else if !self.pads.noted.contains(&id) {
                self.pads.noted.push(id);
                self.install_note(format!("pad {} on the dialog ignored: a pad may only refuse (further presses of this pad are not noted)", p.name()));
            }
            return;
        }
        // The boot scene keeps its input (DS-5); a pad does not open the desktop early.
        if self.images.boot.shown() {
            return;
        }
        self.input_origin = origin;
        if p == Press::Btn(Btn::Home) {
            return self.library_toggle("pad Home");
        }
        if !self.lib.open {
            return;
        }
        if self.menu.is_some() {
            return self.pad_menu(p);
        }
        if self.lib.search_focus {
            // Back one level (sofa-mode.md §4): Down or B returns to the tiles.
            if matches!(p, Press::Move(Dir::Down) | Press::Btn(Btn::B)) {
                self.lib.search_focus = false;
                self.needs_frame = true;
            }
            return;
        }
        match p {
            Press::Move(d) => self.library_move(match d {
                Dir::Left => Move::Left,
                Dir::Right => Move::Right,
                Dir::Up => Move::Up,
                Dir::Down => Move::Down,
            }),
            Press::Btn(Btn::A) => self.library_start_focused(),
            Press::Btn(Btn::B) => self.library_close("pad B"),
            Press::Btn(Btn::X) => self.library_menu_on_focus(),
            Press::Btn(Btn::Y) => {
                self.lib.search_focus = true;
                eprintln!("bitscomp: library: search focused (pad Y; the on-screen keyboard comes with GM-6)");
                self.needs_frame = true;
            }
            _ => {}
        }
    }

    /// The circle menu by pad: a direction lights the item there (up, right, down, left), A runs it, B closes.
    fn pad_menu(&mut self, p: Press) {
        let Some(m) = self.menu.as_mut() else { return };
        match p {
            Press::Move(d) => {
                let i = match d {
                    Dir::Up => 0,
                    Dir::Right => 2,
                    Dir::Down => 4,
                    Dir::Left => 6,
                };
                if m.items[i].is_some() && m.hover != Some(i) {
                    m.hover = Some(i);
                    self.needs_frame = true;
                }
            }
            Press::Btn(Btn::A) => {
                if let Some(i) = m.hover {
                    self.run_menu_item(i);
                }
            }
            Press::Btn(Btn::B) | Press::Btn(Btn::X) => {
                self.menu = None;
                self.needs_frame = true;
            }
            _ => {}
        }
    }

    /// `ctl pad`: every pad, its mapping source, Home, buttons and last press.
    pub fn pad_list(&self) -> String {
        let mut out = format!("pads {}\n", self.pads.list.len());
        for (id, d) in &self.pads.list {
            let buttons: Vec<String> = d.pad.map.buttons.iter().map(|(c, b)| format!("{}:{c:#x}", Press::Btn(*b).name())).collect();
            let last = d.last.map_or("-".to_string(), |(n, t)| format!("{n}@{t}"));
            out.push_str(&format!(
                "pad {id} path={} name=\"{}\" id={:04x}:{:04x} mapping={} home=\"{}\" origin={} last={last} buttons={}\n",
                d.label,
                d.name.trim(),
                d.id.0,
                d.id.1,
                d.pad.map.source.name(),
                d.home(),
                d.origin.name(),
                buttons.join(",")
            ));
        }
        out.push_str("end");
        out
    }

    /// Test path (headless build, only through `phys`): `plug dragonrise|xbox|stick2`, `unplug`, or raw events
    /// `key CODE VALUE` / `abs CODE VALUE` ... for the newest test pad, followed by a SYN_REPORT.
    fn pad_event_ctl(&mut self, w: &[&str]) -> Result<String, String> {
        if !(cfg!(feature = "nested") && self.headless && self.ctl_origin == Origin::Physical) {
            return Err("pad-event: a test command, only as `phys pad-event` in the headless test build".into());
        }
        let newest = self.pads.list.iter().rev().find(|(_, d)| d.label.starts_with("test:")).map(|(k, _)| *k);
        match w.first().copied() {
            Some("plug") => {
                let d = test_pad(w.get(1).copied().unwrap_or("")).ok_or("pad-event plug dragonrise|xbox|stick2")?;
                let id = next_id();
                self.pad_plug(id, d);
                Ok(format!("pad {id}"))
            }
            Some("unplug") => {
                let id = newest.ok_or("no test pad")?;
                self.pad_gone(id);
                Ok(format!("pad {id} gone"))
            }
            Some(_) => {
                let id = newest.ok_or("no test pad: pad-event plug KIND first")?;
                let mut evs = Vec::new();
                for c in w.chunks(3) {
                    let ty = match c[0] {
                        "key" => crate::backend::evdev_map::EV_KEY,
                        "abs" => crate::backend::evdev_map::EV_ABS,
                        o => return Err(format!("pad-event: {o}? (key|abs CODE VALUE)")),
                    };
                    let code = u16::try_from(num(c.get(1))?).map_err(|e| e.to_string())?;
                    let value = i32::try_from(num(c.get(2))?).map_err(|e| e.to_string())?;
                    evs.push((ty, code, value));
                }
                evs.push((crate::backend::evdev_map::EV_SYN, crate::backend::evdev_map::SYN_REPORT, 0));
                self.pad_feed(id, &evs);
                Ok(self.pad_list())
            }
            None => Err("pad-event plug KIND | unplug | key|abs CODE VALUE ...".into()),
        }
    }
}
