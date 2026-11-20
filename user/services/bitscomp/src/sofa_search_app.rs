//! GM-6d: the sofa mode's search screen in bitscomp (plan/spec/sofa-mode.md §2.6, §3.2, §4): opened by Y, the
//! header's search button or a letter typed on any sofa screen; the hits from the cached library index (B4: worked
//! out when the text changes, never a scan here); the pad's keys on it (A presses, X deletes, Y is space, LB/RB move
//! the cursor, Start is Suchen, A held repeats), the real keyboard typing into the same field (held keys repeat at
//! the same 500/33 ms); and the same keyboard on Einstellungen › Netz for `net.wifi.ssid` and the secret
//! `net.wifi.psk` (shown as dots, never logged, never stored outside settingsd). Pure rules in sofa_search.rs.

use mcpreg::Json;
use smithay::input::keyboard::{Keycode, Keysym, ModifiersState};

use crate::pad::{Btn, Press};
use crate::sofa_focus::{self as focus, Screen};
use crate::sofa_layout as lay;
use crate::sofa_model::{self as model, Tile};
use crate::sofa_search::{self as se, Field, Key, KeyHeld, Repeat, Search, Target};
use crate::state::App;

/// The search hit card's look (sofa_search_draw.rs).
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub label: String,
    pub name: String,
    pub colour: Option<usize>,
    pub greyed: bool,
}

fn card(h: &model::Hit) -> Card {
    let label = if h.game { "SPIEL" } else { "PROGRAMM" };
    let label = if h.tile.greyed && !h.tile.meta.is_empty() { format!("{label} · {}", h.tile.meta.to_uppercase()) } else { label.to_string() };
    Card { label, name: h.tile.name.clone(), colour: h.tile.colour, greyed: h.tile.greyed }
}

impl App {
    fn sofa_on_search(&self) -> bool {
        self.sofa.nav.screen == Screen::Suche && !self.sofa.nav.quick && self.sofa.confirm.is_none()
    }

    /// The search screen with an empty field (or `text`, a settings field's value); not while the quick menu or a
    /// question is shown (the screen must not change under them), nothing again when it is already shown.
    pub(crate) fn sofa_search_open(&mut self, target: Target, text: &str) {
        // The learn screen keeps every key until it is done or cancelled (GM-6c).
        if self.sofa.nav.quick || self.sofa.confirm.is_some() || self.sofa.set.learn.is_some() || self.sofa.nav.screen == Screen::Suche {
            return;
        }
        let what = match &target {
            Target::Search => "the search".to_string(),
            Target::Setting { key, .. } => format!("the keyboard for {key}"),
        };
        self.sofa.search = Search { field: Field::of(text), target, ..Search::default() };
        self.sofa_search_refresh();
        self.sofa_go(Screen::Suche);
        eprintln!("bitscomp: sofa: {what} opened");
    }

    /// Einstellungen › Netz: the keyboard for the row's key (the secret starts empty: it is never read back).
    pub(crate) fn sofa_search_edit(&mut self, key: &str) {
        let secret = crate::settings_write::SECRETS.contains(&key);
        let title = if key == "net.wifi.ssid" { "WLAN-Name" } else { "WLAN-Passwort" };
        let text = if secret { String::new() } else { self.settings.text(key).unwrap_or("").to_string() };
        self.sofa_search_open(Target::Setting { key: key.into(), title: title.into(), secret }, &text);
    }

    /// A letter for the field (the real keyboard, a key of the on-screen one), up to `sofa::TYPED_MAX` characters.
    pub(crate) fn sofa_type(&mut self, c: char) {
        let f = &mut self.sofa.search.field;
        let was = f.full;
        if !f.insert(c) && !was {
            eprintln!("bitscomp: sofa: the search field holds {} characters: further letters are dropped", crate::sofa::TYPED_MAX);
        }
        self.sofa_search_refresh();
    }

    /// The hits again when the text changed (the cached index; B4), the focus kept when it is still there.
    fn sofa_search_refresh(&mut self) {
        self.needs_frame = true;
        let s = &self.sofa.search;
        if s.target != Target::Search || s.hits_for.as_deref() == Some(s.field.text.as_str()) {
            return;
        }
        let text = s.field.text.clone();
        let hits = self.sofa_with(|i| model::search_hits(i, &text));
        let s = &mut self.sofa.search;
        s.hits = hits;
        s.hits_for = Some(text);
        s.first = 0;
        s.note = None;
        if self.sofa.nav.screen == Screen::Suche && !self.sofa.focus.as_deref().is_some_and(|f| focus::index(&self.sofa_items(), f).is_some()) {
            self.sofa.focus = Some("key:1".into());
        }
    }

    /// The search screen's focus elements.
    pub fn sofa_search_items(&self) -> Vec<focus::Item> {
        let (bw, bh) = self.sofa_frame().board();
        let s = &self.sofa.search;
        lay::search(bw, bh, s.shows_hits(), s.more(), s.first, s.level, s.target.enter())
    }

    /// The tile of hit card `n` (0 the best program, 1 YouTube, 2 Brave, 3.. the further programs).
    fn sofa_search_tile(&self, n: usize) -> Option<Tile> {
        let s = &self.sofa.search;
        match n {
            1 => Some(self.sofa_with(model::youtube)),
            2 => Some(self.sofa_with(|i| model::brave_search(i, &s.field.text))),
            0 => s.hits.first().map(|h| h.tile.clone()),
            n => s.hits.get(n - 2).map(|h| h.tile.clone()),
        }
    }

    /// The cards of the hit rows (sofa_search_draw.rs), in the items' order.
    pub fn sofa_search_cards(&self) -> Vec<Card> {
        let s = &self.sofa.search;
        if !s.shows_hits() {
            return Vec::new();
        }
        let yt = self.sofa_with(model::youtube);
        let web = self.sofa_with(|i| model::brave_search(i, &s.field.text));
        let best = s.hits.first().map_or_else(|| Card { label: "PROGRAMM".into(), name: "Nichts gefunden".into(), colour: None, greyed: true }, card);
        let yt_label = if yt.greyed { format!("YOUTUBE · {}", yt.meta.to_uppercase()) } else { "YOUTUBE".into() };
        let mut v = vec![
            best,
            Card { label: yt_label, name: format!("»{}«", s.field.text.trim()), colour: None, greyed: yt.greyed },
            Card { label: "WEB · BRAVE".into(), name: web.name.clone(), colour: None, greyed: web.greyed },
        ];
        v.extend(s.hits.iter().skip(1).map(card));
        v
    }

    /// The focus moved: a card of the second row comes into view.
    pub(crate) fn sofa_search_follow(&mut self) {
        if let Some(n) = self.sofa.focus.as_deref().and_then(|f| f.strip_prefix("hit:")).and_then(|n| n.parse::<usize>().ok())
            && n >= 3
        {
            self.sofa.search.follow(n - 3);
            self.needs_frame = true;
        }
    }

    /// A on a search element (a key, a hit card); false for other ids.
    pub(crate) fn sofa_search_activate(&mut self, id: &str) -> bool {
        if self.sofa.nav.screen != Screen::Suche {
            return false;
        }
        if let Some(label) = id.strip_prefix("key:") {
            let label = label.to_string();
            self.sofa_search_key_press(&label);
            return true;
        }
        let Some(n) = id.strip_prefix("hit:").and_then(|n| n.parse::<usize>().ok()) else { return false };
        match self.sofa_search_tile(n) {
            Some(t) => {
                eprintln!("bitscomp: sofa: search hit {n} ({}) chosen", t.name);
                self.sofa_run(&t);
                self.sofa.search.note = self.sofa.body.note.take();
            }
            None => self.sofa.search.note = Some(format!("Kein Programm passt zu „{}“.", self.sofa.search.field.text.trim())),
        }
        self.needs_frame = true;
        true
    }

    /// A key of the on-screen keyboard.
    fn sofa_search_key_press(&mut self, label: &str) {
        let Some(k) = se::key(label) else { return };
        let s = &mut self.sofa.search;
        match k {
            Key::Char(c) => {
                let c = se::shifted(c, s.shift);
                s.shift = false;
                self.sofa_type(c);
            }
            Key::Space => self.sofa_type(' '),
            Key::Delete => {
                s.field.backspace();
                self.sofa_search_refresh();
            }
            Key::Shift => s.shift = !s.shift,
            Key::Level => {
                s.level = if s.level == lay::Level::Letters { lay::Level::Symbols } else { lay::Level::Letters };
                s.shift = false;
                self.sofa.focus = Some(if s.level == lay::Level::Letters { "key:&123" } else { "key:abc" }.into());
            }
            Key::Enter => self.sofa_search_enter(),
        }
        self.needs_frame = true;
    }

    /// Suchen (Start, the key, Enter on a key): the focus goes to the best hit; on a settings field Fertig writes
    /// the value through settingsd and goes back to its row.
    fn sofa_search_enter(&mut self) {
        // DB3: the keyboard typed a follow-up for Durchblick.
        if self.db_sofa_enter() {
            return;
        }
        let Target::Setting { key, secret, .. } = self.sofa.search.target.clone() else {
            if self.sofa.search.shows_hits() {
                self.sofa.focus = Some("hit:0".into());
            }
            return;
        };
        let text = self.sofa.search.field.text.clone();
        if text.is_empty() {
            // An empty field is Abbrechen: the password is never read back, so writing it would wipe the stored one.
            eprintln!("bitscomp: sofa: {key} not changed: the field was empty");
            self.sofa_pad(Press::Btn(Btn::B));
            self.sofa.set.note = Some("Nicht geändert: das Feld war leer.".to_string());
            return self.sofa_search_back_to_row(&key);
        }
        if secret {
            // A secret only through settingsd: never logged, never kept in bitscomp's own values.
            let req = crate::settings_write::Req::set(&key, Json::str(&text));
            let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(crate::sofa_settings::SOFA_PID, req));
            eprintln!("bitscomp: sofa: {key} ({}; the value is not logged)", if sent { "sent to settingsd" } else { "not set: settingsd not there" });
            self.sofa.set.note = (!sent).then(|| "Das WLAN-Passwort geht nur mit dem Einstellungsdienst.".to_string());
        } else {
            self.sofa_settings_set(&key, Json::str(&text));
        }
        let note = self.sofa.set.note.take();
        self.sofa_pad(Press::Btn(Btn::B));
        self.sofa.set.note = note;
        self.sofa_search_back_to_row(&key);
    }

    fn sofa_search_back_to_row(&mut self, key: &str) {
        let row = self.sofa_set_rows().iter().position(|r| matches!(&r.act, crate::sofa_settings::Act::Key(k) if *k == key));
        if let Some(n) = row {
            self.sofa.focus = Some(format!("row:{n}"));
        }
    }

    /// Off the search screen a typed secret is dropped at once (it lives only until Fertig, B or leaving).
    pub(crate) fn sofa_search_forget_secret(&mut self) {
        if self.sofa.search.target.secret() {
            self.sofa.search = Search::default();
        }
    }

    /// A pad press on the search screen; false for the ones that act as on every screen (moves, B, Home).
    pub(crate) fn sofa_search_pad(&mut self, p: Press) -> bool {
        if !self.sofa_on_search() {
            return false;
        }
        let f = &mut self.sofa.search.field;
        match p {
            Press::Btn(Btn::A) => {
                let id = self.sofa.focus.clone();
                if let Some(id) = &id {
                    self.sofa_activate(id);
                }
                // Holding A repeats a key that types or deletes, as a key does.
                let repeats = id.as_deref().and_then(|i| i.strip_prefix("key:")).and_then(se::key).is_some_and(|k| matches!(k, Key::Char(_) | Key::Space | Key::Delete));
                self.sofa.search.pad = id.filter(|_| repeats && self.sofa.nav.screen == Screen::Suche).map(|i| (i, Repeat::start(crate::pacer::now())));
            }
            Press::Btn(Btn::X) => {
                f.backspace();
                self.sofa_search_refresh();
            }
            Press::Btn(Btn::Y) => self.sofa_type(' '),
            Press::Btn(Btn::LB) => f.step(-1),
            Press::Btn(Btn::RB) => f.step(1),
            Press::Btn(Btn::Start) => self.sofa_search_enter(),
            Press::Btn(Btn::Select) => {}
            Press::Btn(Btn::B | Btn::Home | Btn::Help) | Press::Move(_) => return false,
        }
        self.needs_frame = true;
        true
    }

    /// A real key on the search screen; false for the ones that act as on every screen (arrows, Esc).
    pub(crate) fn sofa_search_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        if !self.sofa_on_search() {
            return false;
        }
        let f = &mut self.sofa.search.field;
        match sym {
            Keysym::BackSpace => f.backspace(),
            Keysym::Delete | Keysym::KP_Delete => f.delete(),
            Keysym::Home | Keysym::KP_Home => {
                f.home();
                true
            }
            Keysym::End | Keysym::KP_End => {
                f.end();
                true
            }
            Keysym::Prior => {
                f.step(-1);
                true
            }
            Keysym::Next => {
                f.step(1);
                true
            }
            Keysym::Return | Keysym::KP_Enter => {
                match self.sofa.focus.clone() {
                    Some(id) if id.starts_with("hit:") => self.sofa_activate(&id),
                    _ => self.sofa_search_enter(),
                }
                return true;
            }
            _ => match sym.key_char() {
                Some(c) if !c.is_control() && !mods.ctrl && !mods.alt && !mods.logo => {
                    self.sofa_type(c);
                    return true;
                }
                _ => return false,
            },
        };
        self.sofa_search_refresh();
        true
    }

    /// A real key taken by the sofa layer went down: on the search screen it repeats while held (500 ms, then
    /// every 33 ms); a new key replaces it.
    pub fn sofa_hold(&mut self, code: Keycode, sym: Keysym, mods: ModifiersState) {
        if self.sofa.listens() && self.sofa_on_search() && crate::address::repeats(sym) {
            self.sofa.search.held = Some(KeyHeld { code, sym, mods, rep: Repeat::start(crate::pacer::now()) });
        }
    }

    pub fn sofa_release(&mut self, code: Keycode) {
        if self.sofa.search.held.is_some_and(|h| h.code == code) {
            self.sofa.search.held = None;
        }
    }

    /// The held A (while a pad still holds it and the focus stays on its key) and the held real key repeat.
    pub(crate) fn sofa_search_tick(&mut self, now: std::time::Duration) {
        if !self.sofa.listens() || !self.sofa_on_search() {
            self.sofa.search.pad = None;
            self.sofa.search.held = None;
            return;
        }
        if let Some((id, mut rep)) = self.sofa.search.pad.take() {
            let held = self.pads.list.iter().any(|(_, d)| d.pad.holds(Btn::A));
            if held && self.sofa.focus.as_deref() == Some(id.as_str()) {
                if rep.fire(now) {
                    self.sofa_activate(&id);
                }
                self.sofa.search.pad = Some((id, rep));
            }
        }
        if let Some(mut h) = self.sofa.search.held
            && h.rep.fire(now)
        {
            self.sofa.search.held = Some(h);
            self.sofa_key(h.sym, &h.mods);
        }
    }

    /// `ctl sofa state`'s words of the search (the text URL-encoded: one word).
    pub fn sofa_search_state(&self) -> String {
        let s = &self.sofa.search;
        let text = if s.target.secret() { format!("({}_hidden)", s.field.len()) } else { se::url_encode(&s.field.text) };
        let target = match &s.target {
            Target::Search => "search",
            Target::Setting { key, .. } => key.as_str(),
        };
        format!(
            " search={text} cursor={} target={target} level={} shift={} hits={} hit0={}",
            s.field.cursor,
            if s.level == lay::Level::Letters { "letters" } else { "symbols" },
            if s.shift { "on" } else { "off" },
            s.hits.len(),
            s.hits.first().map_or("-", |h| h.tile.key.as_str()),
        )
    }

    /// The search screen's look (sofa_search_draw.rs).
    pub fn sofa_search_look(&self) -> crate::sofa_search_draw::SearchLook {
        let (bw, bh) = self.sofa_frame().board();
        let s = &self.sofa.search;
        let (title, secret) = match &s.target {
            Target::Search => (String::new(), false),
            Target::Setting { title, secret, .. } => (title.clone(), *secret),
        };
        crate::sofa_search_draw::SearchLook {
            frame: self.sofa_frame(),
            bw,
            bh,
            text: s.field.text.clone(),
            cursor: s.field.cursor,
            title,
            secret,
            cards: self.sofa_search_cards(),
            first: s.first,
            items: self.sofa_search_items(),
            focus: self.sofa.focus.clone(),
            shift: s.shift,
            note: s.note.clone().or_else(|| self.sofa.set.note.clone().filter(|_| s.target != Target::Search)),
        }
    }
}
