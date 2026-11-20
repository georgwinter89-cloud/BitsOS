//! The sidebar's model list in bitscomp (`assist_menu` is its pure part): shown right of the assistant's name
//! while a provider is ready, for the provider the assistant's status names as answering (`ai.role.chat` or the
//! fallback it uses). A pick sets that provider's `ai.provider.<id>.model` through settingsd as the page "KI"'s
//! model field does (a plain set; any input may, injected input too, as there). assist reads a provider's
//! settings when a question starts, so a pick while an answer runs changes the next question, not that answer.

use mcpreg::Json;
use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::assist::Gate;
use crate::assist_layout::Part;
use crate::assist_menu::{self as menu, Entry, Place, Widths};
use crate::chrome::logical_width;
use crate::desk::Spot;
use crate::input::Origin;
use crate::layout::Rect;
use crate::settings_write::{Done, Req};
use crate::state::App;
use crate::text::{Family, Weight};
use crate::theme::{Style as Label, Theme};

/// settingsd's answers to the list's changes come back under this page id (no page asked).
pub const PID: u32 = u32::MAX - 1;
/// Every word the panel's state label can show: the control keeps room for the widest, so it never moves.
const WORDS: [&str; 6] = ["BEREIT", "HÖRT ZU", "DENKT", "ARBEITET", "WARTET AUF DICH", "ANTWORTET"];

#[derive(Default)]
pub struct Menu {
    pub open: bool,
    /// The row the keyboard is on.
    pub pick: usize,
    /// The helper path the announced models were looked up for, and those models.
    path: Option<String>,
    models: Vec<(String, String)>,
    /// The label of the model sent to settingsd, for its answer (which comes before the setting's echo).
    sent: Option<String>,
    /// Where the descriptions are read; None: the owner's home (`helpers::dir`). Tests point it elsewhere.
    dir: Option<std::path::PathBuf>,
}

/// The provider the list is for and what it offers now.
#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    pub id: String,
    pub current: String,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuLook {
    pub place: Place,
    pub label: String,
    pub open: bool,
    pub hover: Option<Part>,
    /// Each row's label and whether it is the model set now.
    pub rows: Vec<(String, bool)>,
    pub pick: usize,
}

pub fn label_font(t: &Theme) -> crate::text::Font {
    crate::chrome_draw::font(t, 12.0, Weight::Medium)
}
pub fn row_font(t: &Theme) -> crate::text::Font {
    crate::chrome_draw::font(t, 13.0, Weight::Regular)
}

/// The same program file: the same path, or both resolve to one file (a link).
fn same_file(a: &str, b: &str) -> bool {
    a == b || matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

impl App {
    fn assist_menu_kind(&self, id: &str) -> (String, String) {
        let text = |f: &str| self.settings.text(&crate::ki::key_of(id, f)).unwrap_or("").trim().to_string();
        (text("kind"), text("address"))
    }

    /// The list is for the provider that answers; None: nothing to show (none ready, no settings service).
    pub fn assist_menu_shown(&self) -> Option<Shown> {
        if !self.assist_on() || !self.settings_live || self.assist.gate() != Gate::Ready {
            return None;
        }
        let id = self.assist.status.as_ref()?.provider.clone()?;
        if !crate::ki::ids(self.settings.text(crate::ki::LIST).unwrap_or("")).contains(&id) {
            return None;
        }
        let (kind, address) = self.assist_menu_kind(&id);
        let m = &self.assist_menu;
        let helper = if m.path.as_deref() == Some(address.as_str()) { m.models.as_slice() } else { &[] };
        let current = self.settings.text(&crate::ki::key_of(&id, "model")).unwrap_or("").trim().to_string();
        let entries = menu::entries(&current, &menu::offered(&kind, helper));
        Some(Shown { id, current, entries })
    }

    /// Where the control and its open list lie, with what they show.
    pub fn assist_menu_place(&self) -> Option<(Shown, Place)> {
        let shown = self.assist_menu_shown()?;
        let p = self.assist_place()?;
        let t = &self.theme;
        let s = t.scale;
        let (band, all, name_w, right) = match p.panel {
            Some(r) => {
                let band = Rect { x: r.x + t.bl(24.0), y: r.y + t.bl(22.0), w: r.w - 2 * t.bl(24.0), h: t.bl(24.0) };
                let lf = t.style(Label::Label);
                let right = WORDS.iter().map(|w| logical_width(&lf, &Theme::label(Label::Label, w), s)).max().unwrap_or(0);
                (band, r, logical_width(&t.bfont(Family::Display, 20.0, Weight::SemiBold), &self.assist.name, s), right)
            }
            None => {
                let r = self.chrome().side?;
                let tx = r.x + 16 + 44 + 12;
                let band = Rect { x: tx, y: r.y + 20, w: r.w - (tx - r.x) - 16, h: 22 };
                (band, r, logical_width(&crate::chrome_draw::font(t, 16.0, Weight::SemiBold), &self.assist.name, s), 0)
            }
        };
        let (lf, rf) = (label_font(t), row_font(t));
        let label = self.assist_menu_label(&shown);
        let w = Widths {
            name: name_w,
            right,
            label: logical_width(&lf, &label, s),
            rows: shown.entries.iter().map(|e| logical_width(&rf, e.label(), s)).collect(),
        };
        let place = menu::place(band, all, &w, self.assist_menu.open)?;
        Some((shown, place))
    }

    fn assist_menu_label(&self, shown: &Shown) -> String {
        menu::current(&shown.entries, &shown.current).map_or(shown.current.clone(), |i| shown.entries[i].label().to_string())
    }

    pub fn assist_menu_look(&self) -> Option<MenuLook> {
        let (shown, place) = self.assist_menu_place()?;
        let now = menu::current(&shown.entries, &shown.current);
        let hover = match self.desk.hover {
            Some(Spot::Assist(p @ (Part::Model | Part::ModelRow(_)))) => Some(p),
            _ => None,
        };
        Some(MenuLook {
            label: self.assist_menu_label(&shown),
            open: self.assist_menu.open,
            hover,
            rows: shown.entries.iter().enumerate().map(|(i, e)| (e.label().to_string(), now == Some(i))).collect(),
            pick: self.assist_menu.pick,
            place,
        })
    }

    /// The list's part under (x, y), before the conversation's.
    pub fn assist_menu_at(&self, x: f64, y: f64) -> Option<Part> {
        let (_, p) = self.assist_menu_place()?;
        if p.closed.contains(x, y) {
            return Some(Part::Model);
        }
        if let Some(i) = p.rows.iter().position(|r| r.contains(x, y)) {
            return Some(Part::ModelRow(i));
        }
        p.frame.contains(x, y).then_some(Part::Body)
    }

    pub fn assist_menu_open(&self) -> bool {
        self.assist_menu.open && self.assist_menu_shown().is_some()
    }

    /// After every change of the assistant or its settings: a helper's announced models are looked up when its
    /// path is new, and an open list whose provider is gone closes.
    pub(crate) fn assist_menu_follow(&mut self) {
        let Some(shown) = self.assist_menu_shown() else {
            return self.assist_menu_close("the assistant cannot answer now");
        };
        let (kind, address) = self.assist_menu_kind(&shown.id);
        if kind == crate::ki::PROGRAM && self.assist_menu.path.as_deref() != Some(address.as_str()) {
            self.assist_menu_scan(&address);
        }
    }

    /// Reads the helper descriptions (nothing is run) for the models the helper at `address` announces.
    fn assist_menu_scan(&mut self, address: &str) {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let dir = self.assist_menu.dir.clone().unwrap_or_else(|| crate::helpers::dir(uid));
        let found = crate::helpers::scan(&dir, uid).into_iter().find(|h| same_file(&h.path, address));
        self.assist_menu.models = found.map(|h| h.models).unwrap_or_default();
        self.assist_menu.path = Some(address.to_string());
    }

    pub fn assist_menu_toggle(&mut self, why: &str) {
        if self.assist_menu.open {
            return self.assist_menu_close(why);
        }
        let Some(shown) = self.assist_menu_shown() else { return };
        let (kind, address) = self.assist_menu_kind(&shown.id);
        if kind == crate::ki::PROGRAM {
            // A description may have changed since the last look.
            self.assist_menu_scan(&address);
        }
        self.address_end();
        self.page_edit_end();
        self.assist_blur();
        self.desk.min_list.open = false;
        let shown = self.assist_menu_shown().unwrap_or(shown);
        self.assist_menu.pick = menu::current(&shown.entries, &shown.current).unwrap_or(0);
        self.assist_menu.open = true;
        eprintln!("bitscomp: assist: model list opened ({why}) for {} with {} entries", shown.id, shown.entries.len());
        self.chrome_dirty();
    }

    pub fn assist_menu_close(&mut self, why: &str) {
        if self.assist_menu.open {
            self.assist_menu.open = false;
            eprintln!("bitscomp: assist: model list closed ({why})");
            self.chrome_dirty();
        }
    }

    /// A press anywhere but on the control or its list closes the list (the press goes on as usual).
    pub fn assist_menu_press(&mut self) {
        if self.assist_menu.open && !matches!(self.assist_menu_at(self.pointer.x, self.pointer.y), Some(_)) {
            self.assist_menu_close("a click outside");
        }
    }

    /// Row `i` chosen (a click, Enter, `ctl assist model pick`).
    pub fn assist_menu_pick(&mut self, i: usize, origin: Origin) {
        let Some(shown) = self.assist_menu_shown() else { return };
        let Some(entry) = shown.entries.get(i).cloned() else { return };
        self.assist_menu_close("picked");
        let label = entry.label().to_string();
        let value = match entry {
            Entry::More => {
                let to = format!("{}/{}/model", crate::page_kind::KI, shown.id);
                eprintln!("bitscomp: assist: model list opens {to}");
                return self.open_address(&to, crate::address::Where::Tab, None);
            }
            Entry::Model { value, .. } if value == shown.current => return,
            Entry::Model { value, .. } => value,
        };
        let key = crate::ki::key_of(&shown.id, "model");
        let from = if origin == Origin::Physical { "" } else { " (injected input)" };
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(PID, Req::set(&key, Json::str(&value))));
        if sent {
            eprintln!("bitscomp: assist: model of {} set to {value:?}{from}", shown.id);
            self.assist_menu.sent = Some(label);
        } else {
            self.assist.note = Some(("Der Einstellungsdienst antwortet nicht: Modell nicht geändert.".into(), true));
        }
        self.chrome_dirty();
    }

    /// settingsd answered a pick: the line above the input says what holds now.
    pub fn assist_menu_done(&mut self, d: Done) {
        let label = self.assist_menu.sent.take().unwrap_or_default();
        let text = if d.ok && d.ask.is_none() {
            (format!("Modell: {label}. Gilt ab der nächsten Frage."), false)
        } else {
            eprintln!("bitscomp: assist: settingsd refused the model: {}", d.text);
            (format!("Modell nicht geändert: {}", d.text), true)
        };
        self.assist.note = Some(text);
        self.chrome_dirty();
    }

    /// A key while the list is open (it takes every key but the modifiers); false: not the list's.
    pub fn assist_menu_key(&mut self, sym: Keysym, _mods: &ModifiersState, origin: Origin) -> bool {
        if crate::library_ui::is_modifier(sym) {
            return false;
        }
        let n = self.assist_menu_shown().map_or(0, |s| s.entries.len());
        let pick = self.assist_menu.pick;
        match sym {
            Keysym::Escape => self.assist_menu_close("Esc"),
            Keysym::Tab | Keysym::ISO_Left_Tab => self.assist_menu_close("Tab"),
            Keysym::Return | Keysym::KP_Enter => self.assist_menu_pick(pick, origin),
            _ => {
                self.assist_menu.pick = match sym {
                    Keysym::Up | Keysym::KP_Up => pick.saturating_sub(1),
                    Keysym::Down | Keysym::KP_Down => (pick + 1).min(n.saturating_sub(1)),
                    Keysym::Home | Keysym::KP_Home => 0,
                    Keysym::End | Keysym::KP_End => n.saturating_sub(1),
                    _ => pick,
                }
            }
        }
        self.chrome_dirty();
        true
    }

    /// The `ctl hot` lines of the control and, while open, its rows.
    pub fn assist_menu_hot(&self) -> String {
        let Some((shown, p)) = self.assist_menu_place() else { return String::new() };
        let s = self.theme.scale;
        let mid = |r: Rect| format!("{},{}", ((r.x + r.w / 2) as f64 * s).round(), ((r.y + r.h / 2) as f64 * s).round());
        let now = menu::current(&shown.entries, &shown.current);
        let mut out = format!("assist-model at={} label={:?} open={} provider={}\n", mid(p.closed), self.assist_menu_label(&shown), self.assist_menu.open, shown.id);
        for (i, (r, e)) in p.rows.iter().zip(&shown.entries).enumerate() {
            let value = match e {
                Entry::Model { value, .. } => format!("{value:?}"),
                Entry::More => "more".into(),
            };
            out.push_str(&format!("assist-model row {i} at={} value={value} label={:?} current={}\n", mid(*r), e.label(), now == Some(i)));
        }
        out
    }

    /// `ctl assist model open|close|state|pick I`.
    pub fn assist_menu_ctl(&mut self, words: &[&str]) -> Result<String, String> {
        let origin = self.ctl_origin;
        match words.first().copied() {
            Some("open") if !self.assist_menu.open => self.assist_menu_toggle("ctl"),
            Some("open") => {}
            Some("close") => self.assist_menu_close("ctl"),
            Some("pick") => {
                let Some(i) = words.get(1).and_then(|w| w.parse::<usize>().ok()) else { return Err("usage: assist model pick I".into()) };
                if self.assist_menu_shown().is_none_or(|s| i >= s.entries.len()) {
                    return Err(format!("assist model: no row {i}"));
                }
                self.assist_menu_pick(i, origin);
            }
            Some("state") | None => {}
            _ => return Err("usage: assist model open|close|state|pick I".into()),
        }
        Ok(self.assist_menu_state())
    }

    fn assist_menu_state(&self) -> String {
        match self.assist_menu_shown() {
            None => "model shown=false".into(),
            Some(s) => format!(
                "model shown={} open={} provider={} value={:?} label={:?} rows={} pick={}",
                self.assist_menu_place().is_some(),
                self.assist_menu.open,
                s.id,
                s.current,
                self.assist_menu_label(&s),
                s.entries.len(),
                self.assist_menu.pick
            ),
        }
    }
}

#[cfg(test)]
#[path = "assist_menu_tests.rs"]
mod tests;
