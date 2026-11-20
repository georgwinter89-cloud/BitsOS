//! GM-6c: the sofa mode's Einstellungen (plan/spec/sofa-mode.md 2.5, §6): the six sections and their rows over
//! settingsd (the rows' controls come from settingsd's schema, as the desktop's settings page: WY-9's
//! page_schema), ◀ ▶ change a value, A toggles, X (Rücktaste) undoes the newest change made here (settingsd's
//! undo, B1); confirm-class keys are shown read-only (WY-9's rule: they are changed in the desktop with its
//! question); secrets are never read back (dots). Steuerung lists the pads with their mapping and holds the learn
//! screen ("Tasten zuordnen": A, B, X, Y, LB, RB, Select, Start in turn, stored in `sofa.pad.map`, pad.rs reads
//! it). The rows are a pure function of the inputs (host-tested in sofa_settings_tests.rs).

use mcpreg::Json;

use crate::page::{Kind, Row};
use crate::pad::{Btn, LEARN};
use crate::settings::Values;
use crate::settings_write::{Done, Req};
use crate::sofa_focus::Screen;
use crate::sofa_quick::Confirm;
use crate::state::App;

/// The pseudo page pid of the sofa mode's settings answers (page_done routes them here).
pub const SOFA_PID: u32 = u32::MAX;
pub const DOTS: &str = "••••••••";

/// The control at a row's right (§2.5).
#[derive(Clone, Debug, PartialEq)]
pub enum Ctl {
    /// ‹ value ›; `greyed`: shown, not changeable (Auflösung until the own display engine).
    Arrows { value: String, greyed: bool },
    Switch(bool),
    Value(String),
    /// A row that does something on A (no value).
    Action,
}

/// What a row is.
#[derive(Clone, Debug, PartialEq)]
pub enum Act {
    Key(String),
    Learn,
    Desktop,
    Reboot,
    Off,
    /// Read-only facts (Auflösung, a pad, the link, the version).
    Info,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SRow {
    pub title: String,
    pub line: String,
    pub ctl: Ctl,
    pub act: Act,
}

/// What the rows are made of.
pub struct Input<'a> {
    pub values: &'a Values,
    /// settingsd's rows (page_model); keys missing there come from the sofa mode's own definitions.
    pub rows: &'a [Row],
    pub screen: (i32, i32),
    pub link: Option<&'a str>,
    /// (name, mapping source) of each pad.
    pub pads: Vec<(String, &'static str)>,
    pub date: String,
    /// The running generation (`/proc/bitsos/generation`); None on a kernel without the file.
    pub generation: Option<u32>,
}

/// The learn screen's state: the pad and the buttons pressed so far (raw codes, in LEARN's order).
#[derive(Clone, Debug, PartialEq)]
pub struct Learn {
    pub pad: u64,
    pub got: Vec<(Btn, u16)>,
}

impl Learn {
    /// The button asked for now (None: all done).
    pub fn wants(&self) -> Option<(Btn, &'static str)> {
        LEARN.get(self.got.len()).copied()
    }

    /// A raw press; false when the code is taken already (one code is one button).
    pub fn press(&mut self, code: u16) -> bool {
        let Some((b, _)) = self.wants() else { return false };
        if self.got.iter().any(|(_, c)| *c == code) {
            return false;
        }
        self.got.push((b, code));
        true
    }
}

/// The sofa mode's Einstellungen state (in `Sofa`).
#[derive(Default)]
pub struct Set {
    pub section: usize,
    /// settingsd's answer, a refusal or the learn screen's result (German).
    pub note: Option<String>,
    pub learn: Option<Learn>,
}

/// German words of a pad mapping's source.
pub fn source_word(s: crate::pad::Source) -> &'static str {
    match s {
        crate::pad::Source::Standard => "Standard",
        crate::pad::Source::Table => "Tabelle",
        crate::pad::Source::Fallback => "Vermutet",
        crate::pad::Source::Learned => "Gelernt",
    }
}

/// The settingsd row of `key`: settingsd's own, else from the sofa mode's definitions (§6) when settingsd is not
/// there.
pub fn row_for(key: &str, rows: &[Row]) -> Option<Row> {
    rows.iter().find(|r| r.key == key).cloned().or_else(|| {
        crate::sofa::defs().iter().find(|d| d.get("key").and_then(Json::as_str) == Some(key)).and_then(crate::page_schema::spec_of).map(|s| crate::page_schema::row_of(&s))
    })
}

fn key_row(key: &str, title: &str, line: &str, i: &Input) -> SRow {
    let act = Act::Key(key.into());
    let Some(r) = row_for(key, i.rows) else {
        return SRow { title: title.into(), line: format!("{line} · der Einstellungsdienst kennt sie nicht"), ctl: Ctl::Value("–".into()), act };
    };
    let v = i.values.text(key).map(str::to_string).unwrap_or_else(|| r.default.clone());
    let shown = |k: &Kind| match k {
        Kind::Choice(o) => o.iter().find(|(x, _)| *x == v).map_or(v.clone(), |(_, n)| n.clone()),
        Kind::Number { unit, .. } if !unit.is_empty() => format!("{v} {unit}"),
        Kind::Switch => if v == "true" { "An".into() } else { "Aus".into() },
        _ if v.is_empty() => "–".into(),
        _ => v.clone(),
    };
    if r.confirm {
        return SRow { title: title.into(), line: format!("{line} · nur im Desktop änderbar (mit Rückfrage)"), ctl: Ctl::Value(shown(&r.kind)), act };
    }
    let ctl = match &r.kind {
        Kind::Switch => Ctl::Switch(v == "true"),
        Kind::Number { .. } | Kind::Choice(_) => Ctl::Arrows { value: shown(&r.kind), greyed: false },
        // CL-5: the last three are rows of the page "KI" only (never in the registry's rows).
        Kind::Fixed(_) | Kind::Pills { .. } | Kind::Secret { .. } | Kind::Buttons(_) | Kind::Link(_) | Kind::Chart => Ctl::Value(DOTS.into()),
        // WY-SET: rows of the settings tabs only (never in the registry's rows).
        Kind::Fold { .. } | Kind::Subs(_) => Ctl::Value(DOTS.into()),
        Kind::Field { .. } | Kind::Accent => Ctl::Value(shown(&r.kind)),
    };
    SRow { title: title.into(), line: line.into(), ctl, act }
}

fn info(title: &str, line: &str, value: &str) -> SRow {
    SRow { title: title.into(), line: line.into(), ctl: Ctl::Value(value.into()), act: Act::Info }
}

fn action(title: &str, line: &str, act: Act) -> SRow {
    SRow { title: title.into(), line: line.into(), ctl: Ctl::Action, act }
}

/// The rows of section `s` (crate::sofa_layout::SECTIONS' order).
pub fn rows(s: usize, i: &Input) -> Vec<SRow> {
    match s {
        0 => vec![
            SRow {
                title: "Auflösung".into(),
                line: "Was der Fernseher meldet · ändern kommt später".into(),
                ctl: Ctl::Arrows { value: crate::sofa_quick::screen_text(i.screen.0, i.screen.1), greyed: true },
                act: Act::Info,
            },
            key_row("sofa.overscan", "Bildausschnitt", "Für Fernseher, die den Rand abschneiden", i),
            key_row("sofa.start_volume", "Lautstärke beim Start", "Leise anfangen, dann hochdrehen", i),
            key_row("kernel.snd_output", "Tonausgabe", "Wo der Ton spielt", i),
        ],
        1 => {
            let mut v: Vec<SRow> = i.pads.iter().map(|(n, src)| info(n, &format!("Belegung: {src}"), "Verbunden")).collect();
            if v.is_empty() {
                v.push(info("Kein Gamepad", "Schließe ein Gamepad an oder schalte es ein", "–"));
            }
            v.push(action("Tasten zuordnen", "A, B, X, Y, LB, RB, Select und Start nacheinander drücken", Act::Learn));
            v.push(key_row("sofa.home_chord", "Home-Taste", "Für Gamepads ohne Home: Select und Start zusammen", i));
            v
        }
        2 => vec![
            info("Verbindung", "Kabel oder WLAN", &crate::sofa_quick::wlan_text(i.link, None)),
            key_row("net.wifi.ssid", "WLAN-Name", "A öffnet die Bildschirmtastatur; eine echte Tastatur tippt mit", i),
            SRow { title: "WLAN-Passwort".into(), line: "Wird nie angezeigt".into(), ctl: Ctl::Value(DOTS.into()), act: Act::Key("net.wifi.psk".into()) },
        ],
        3 => vec![
            key_row("sofa.start_on_pad", "Im Sofa-Modus starten", "Sobald ein Gamepad eingeschaltet wird", i),
            key_row("sofa.motion", "Bewegung", "Kacheln heben sich, wenn sie gewählt sind", i),
            key_row("sofa.sort", "Sortierung", "Wie die Bibliothek sortiert", i),
        ],
        5 => vec![
            info("BitsOS", &format!("{} · {}", i.date, generation_text(i.generation)), "V2"),
            action("Alle Einstellungen im Desktop", "Wechselt zum Desktop und öffnet die Einstellungen", Act::Desktop),
            action("Neu starten", "Alle Programme werden beendet", Act::Reboot),
            action("Ausschalten", "Alle Programme werden beendet", Act::Off),
        ],
        _ => Vec::new(),
    }
}

/// Konten's lines (text only, nothing stored).
pub const KONTEN: [&str; 2] = ["Steam meldest du in Steam selbst an.", "YouTube in YouTube selbst."];

/// The value ◀ (d < 0) or ▶ (d > 0) gives a key's row; None when nothing changes (an end, read-only).
pub fn change(r: &Row, current: &str, d: i32) -> Option<Json> {
    if r.confirm {
        return None;
    }
    let cur = if current.is_empty() { r.default.as_str() } else { current };
    match &r.kind {
        Kind::Number { min, max, step, .. } => {
            let v = cur.parse::<i64>().unwrap_or(*min);
            // Onto the step's grid from the range's start, then the next step (as the page's − and +).
            let s = (*step).max(1);
            let base = v.saturating_sub(*min);
            let to = if d > 0 { min + (base / s + 1) * s } else { min + ((base + s - 1) / s - 1) * s };
            let to = to.clamp(*min, *max);
            (to != v).then(|| Json::Num(to as f64))
        }
        Kind::Choice(o) => {
            let i = o.iter().position(|(x, _)| x == cur).unwrap_or(0) as i64 + d.signum() as i64;
            let to = o.get(usize::try_from(i).ok()?)?;
            (to.0 != cur).then(|| Json::str(&to.0))
        }
        Kind::Switch => {
            let to = d > 0;
            ((cur == "true") != to).then_some(Json::Bool(to))
        }
        _ => None,
    }
}

fn generation_text(g: Option<u32>) -> String {
    match g {
        Some(n) if n > 0 => format!("Generation {n}"),
        _ => "Generation unbekannt".into(),
    }
}

/// `generation=<n> ...` -> n.
fn parse_generation(line: &str) -> Option<u32> {
    line.split_whitespace().find_map(|w| w.strip_prefix("generation="))?.parse().ok()
}

/// Read once: the generation does not change while the system runs.
fn running_generation() -> Option<u32> {
    static GEN: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *GEN.get_or_init(|| std::fs::read_to_string("/proc/bitsos/generation").ok().and_then(|t| parse_generation(&t)))
}

impl App {
    pub fn sofa_set_input(&self) -> Input<'_> {
        let date = self.status.clock.split(" · ").next().unwrap_or("").to_string();
        Input {
            values: &self.settings,
            rows: &self.page_model.rows,
            screen: crate::chrome_render::wallpaper::output_px(self),
            link: self.status.blocks.iter().find(|b| b.label == "Netz").map(|b| b.value.as_str()),
            pads: self.pads.list.iter().map(|(_, d)| (d.name.trim().to_string(), source_word(d.pad.map.source))).collect(),
            date: if date.is_empty() { "Heute".into() } else { date },
            generation: running_generation(),
        }
    }

    pub fn sofa_set_rows(&self) -> Vec<SRow> {
        rows(self.sofa.set.section, &self.sofa_set_input())
    }

    /// A change of `key` through settingsd (B1: undo works); without settingsd it holds until bitscomp ends.
    pub fn sofa_settings_set(&mut self, key: &str, value: Json) {
        let shown = match &value {
            Json::Str(s) => s.clone(),
            v => v.to_string(),
        };
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(SOFA_PID, Req::set(key, value)));
        if !sent {
            self.settings.set(key, &shown);
            self.apply_settings(key);
            self.sofa.set.note = Some("Nur bis zum Neustart: der Einstellungsdienst antwortet nicht.".into());
        }
        eprintln!("bitscomp: sofa: {key} = {shown} ({})", if sent { "sent to settingsd" } else { "local only: settingsd not there" });
        self.needs_frame = true;
    }

    /// X, the Rücktaste, the footer's Rückgängig: settingsd's undo of the newest change made here.
    pub fn sofa_set_undo(&mut self) {
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(SOFA_PID, Req::UNDO));
        eprintln!("bitscomp: sofa: undo ({})", if sent { "sent to settingsd" } else { "settingsd not there" });
        if !sent {
            self.sofa.set.note = Some("Rückgängig geht nur mit dem Einstellungsdienst.".into());
        }
        self.needs_frame = true;
    }

    /// settingsd answered a change or an undo of the sofa mode.
    pub fn sofa_set_done(&mut self, d: Done) {
        let text = if d.ask.is_some() {
            "Diese Einstellung ändert man im Desktop (mit Rückfrage).".to_string()
        } else {
            let name = d.key.as_deref().map(|k| row_for(k, &self.page_model.rows).map_or(k.to_string(), |r| r.short));
            match name {
                Some(n) => format!("{n}: {}", d.text),
                None => d.text.clone(),
            }
        };
        eprintln!("bitscomp: sofa: settingsd: {text}");
        self.sofa.set.note = Some(text);
        self.needs_frame = true;
    }

    fn sofa_row_at(&self) -> Option<(usize, SRow)> {
        let n = self.sofa.focus.as_deref()?.strip_prefix("row:")?.parse::<usize>().ok()?;
        self.sofa_set_rows().into_iter().nth(n).map(|r| (n, r))
    }

    /// ◀ ▶ on a row with a value (true: taken; Left on a row without one goes to the column as a move).
    pub fn sofa_set_change(&mut self, d: i32) -> bool {
        if self.sofa.nav.screen != Screen::Einstellungen || self.sofa.confirm.is_some() || self.sofa.set.learn.is_some() {
            return false;
        }
        let Some((_, r)) = self.sofa_row_at() else { return false };
        match (&r.ctl, &r.act) {
            (Ctl::Arrows { greyed: true, .. }, _) => {
                self.sofa.set.note = Some("Die Auflösung lässt sich erst mit dem eigenen Bildtreiber ändern.".into());
                self.needs_frame = true;
                true
            }
            (Ctl::Arrows { .. } | Ctl::Switch(_), Act::Key(k)) => {
                if let Some(row) = row_for(k, &self.page_model.rows) {
                    let cur = self.settings.text(k).unwrap_or("").to_string();
                    if let Some(v) = change(&row, &cur, d) {
                        let k = k.clone();
                        self.sofa_settings_set(&k, v);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// A click on row `n` at board x `x`: on ‹ or › of its value that change (true), else as A (false).
    pub fn sofa_set_click(&mut self, n: usize, x: f64, y: f64) -> bool {
        let Some(r) = self.sofa_set_rows().into_iter().nth(n) else { return false };
        let Ctl::Arrows { value, .. } = &r.ctl else { return false };
        let Some(rect) = self.sofa_body_items().into_iter().find(|i| i.id == format!("row:{n}")).map(|i| i.rect) else { return false };
        let (prev, _, next) = crate::sofa_settings_draw::arrows(rect, value);
        let d = if prev.contains(x, y) { -1 } else if next.contains(x, y) { 1 } else { return false };
        self.sofa_set_change(d)
    }

    /// A on Einstellungen's elements; false for others.
    pub fn sofa_set_activate(&mut self, id: &str) -> bool {
        if let Some(n) = id.strip_prefix("section:").and_then(|n| n.parse::<usize>().ok()) {
            self.sofa_set_section(n);
            return true;
        }
        let Some(n) = id.strip_prefix("row:").and_then(|n| n.parse::<usize>().ok()) else { return false };
        let Some(r) = self.sofa_set_rows().into_iter().nth(n) else { return true };
        match (&r.ctl, &r.act) {
            (Ctl::Switch(on), Act::Key(k)) => {
                let k = k.clone();
                self.sofa_settings_set(&k, Json::Bool(!on));
            }
            (_, Act::Learn) => self.sofa_learn_start(),
            (_, Act::Desktop) => {
                self.sofa_switch(crate::sofa::Way::Desktop);
                self.open_address("einstellungen/", crate::address::Where::Tab, None);
            }
            (_, Act::Reboot) => self.sofa_power_ask(Confirm::Reboot),
            (_, Act::Off) => self.sofa_power_ask(Confirm::Off),
            (_, Act::Key(k)) if k.starts_with("net.wifi.") => {
                let k = k.clone();
                self.sofa_search_edit(&k);
            }
            _ => {}
        }
        self.needs_frame = true;
        true
    }

    /// The section shown (the focus in the left column follows it).
    pub fn sofa_set_section(&mut self, n: usize) {
        let n = n.min(crate::sofa_layout::SECTIONS.len() - 1);
        if n != self.sofa.set.section {
            self.sofa.set.section = n;
            self.sofa.set.note = None;
            eprintln!("bitscomp: sofa: Einstellungen › {}", crate::sofa_layout::SECTIONS[n].0);
        }
        self.needs_frame = true;
    }

    /// LB/RB (Bild auf/ab, the wheel) on Einstellungen: the previous/next section.
    pub fn sofa_set_lbrb(&mut self, by: i32) -> bool {
        if self.sofa.nav.screen != Screen::Einstellungen {
            return false;
        }
        let n = (self.sofa.set.section as i32 + by).clamp(0, crate::sofa_layout::SECTIONS.len() as i32 - 1) as usize;
        self.sofa_set_section(n);
        self.sofa.focus = Some(format!("section:{n}"));
        true
    }

    /// Tasten zuordnen: the pad pressed last learns (A is asked first).
    fn sofa_learn_start(&mut self) {
        let pad = self.pads.list.iter().max_by_key(|(_, d)| d.last.map_or(0, |(_, t)| t)).map(|(id, _)| *id);
        let Some(pad) = pad else {
            self.sofa.set.note = Some("Kein Gamepad verbunden.".into());
            return;
        };
        eprintln!("bitscomp: sofa: learning the buttons of pad {pad}");
        self.sofa.set.note = None;
        self.sofa.set.learn = Some(Learn { pad, got: Vec::new() });
    }

    /// Esc, a right click, another pad's B: the learn screen closes, nothing is stored.
    pub fn sofa_learn_cancel(&mut self) -> bool {
        if self.sofa.set.learn.take().is_none() {
            return false;
        }
        eprintln!("bitscomp: sofa: learning cancelled, nothing stored");
        self.sofa.set.note = Some("Abgebrochen: die Belegung bleibt.".into());
        self.needs_frame = true;
        true
    }

    /// A raw button press of the pad that learns.
    pub fn sofa_learn_raw(&mut self, id: u64, code: u16) {
        let Some(l) = self.sofa.set.learn.as_mut().filter(|l| l.pad == id) else { return };
        if code < crate::pad::BTN_MISC {
            return;
        }
        if !l.press(code) {
            self.sofa.set.note = Some("Diese Taste ist schon belegt.".into());
            self.needs_frame = true;
            return;
        }
        self.sofa.set.note = None;
        self.needs_frame = true;
        if l.wants().is_some() {
            return;
        }
        let Some(l) = self.sofa.set.learn.take() else { return };
        let Some((_, d)) = self.pads.list.iter_mut().find(|(k, _)| *k == id) else { return };
        d.pad.reset();
        let text = crate::pad::learned_text(&self.sofa.prefs.pad_map, d.id.0, d.id.1, &d.keys, &l.got);
        eprintln!("bitscomp: sofa: pad {} learned: {text}", d.label);
        self.sofa_settings_set("sofa.pad.map", Json::str(&text));
        if !self.settings_live {
            self.pad_remap();
        }
        self.sofa.set.note = Some("Gelernt: die neue Belegung gilt.".into());
    }

    /// What Einstellungen' body draws.
    pub fn sofa_set_look(&self) -> crate::sofa_settings_draw::SettingsLook {
        let (bw, bh) = self.sofa_frame().board();
        crate::sofa_settings_draw::SettingsLook {
            frame: self.sofa_frame(),
            bw,
            bh,
            section: self.sofa.set.section,
            rows: self.sofa_set_rows(),
            items: self.sofa_body_items(),
            focus: self.sofa.focus.clone(),
            note: self.sofa.set.note.clone(),
            learn: self.sofa.set.learn.as_ref().map(|l| (l.got.len(), l.wants().map_or("", |(_, n)| n))),
            confirm: self.sofa.confirm.filter(|_| !self.sofa.nav.quick),
        }
    }
}

#[cfg(test)]
#[path = "sofa_settings_tests.rs"]
mod tests;
