//! The Feld in bitscomp (feld.rs has the rows, feld_view.rs the places, feld_draw.rs the look): opened by the
//! Bühne's Feld bar, Win+Space (every preset: the classic chrome has no bar, the key opens the same overlay) or
//! `ctl feld open`; closed by Esc, Win+Space again, a click outside the panel or its Esc chip. While open it takes
//! every key but the modifiers, with the address bar's editing and key repeat (`address::Edit`, `address_hold`).
//! Never over the sofa mode's screens, the installer's question or a confirmation dialog; the library overlay and
//! the Feld close each other. Enter (or a click on a row) does the row: an address takes `open_address`, a program
//! `library_start`, a question the sidebar's own submit path (`assist::Model::submit`/`ask`), so the provider, the
//! cards and the history are as if it had been typed there.

use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::address::{self, Open, Where};
use crate::feld::{self, Kind, Row};
use crate::feld_view::{self as view, Geo};
use crate::input::Origin;
use crate::layout::Rect;
use crate::state::App;

impl App {
    /// Why the Feld cannot open now (None: it can).
    pub fn feld_refused(&self) -> Option<&'static str> {
        if self.sofa.listens() {
            Some("the sofa mode's screen listens")
        } else if self.install.shown() || self.images.boot.shown() {
            Some("the installer's question or the boot scene is shown")
        } else if self.desk.ws_ask.is_some() || self.page_dialog_pid().is_some() {
            Some("a confirmation dialog is open")
        } else {
            None
        }
    }

    pub fn feld_toggle(&mut self, why: &str) {
        if self.feld.open { self.feld_close(why) } else { self.feld_open(why) }
    }

    pub fn feld_open(&mut self, why: &str) {
        if self.feld.open {
            return;
        }
        if let Some(no) = self.feld_refused() {
            eprintln!("bitscomp: feld: not opened ({why}): {no}");
            return;
        }
        self.library_close("Feld");
        self.rewind_close("Feld");
        self.assist_menu_close("Feld");
        self.address_end();
        self.page_edit_end();
        self.assist_blur();
        self.menu = None;
        self.desk.min_list.open = false;
        let f = &mut self.feld;
        (f.open, f.edit, f.hover, f.hover_esc, f.remote) = (true, address::Edit::default(), None, false, false);
        self.feld_refresh();
        eprintln!("bitscomp: feld: opened ({why})");
        self.hover = None;
        self.chrome_dirty();
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    pub fn feld_close(&mut self, why: &str) {
        if !self.feld.open {
            return;
        }
        self.feld.open = false;
        self.feld.edit = address::Edit::default();
        self.feld.rows.clear();
        self.desk.held = None;
        eprintln!("bitscomp: feld: closed ({why})");
        self.chrome_dirty();
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    pub fn feld_geo(&self) -> Geo {
        view::geo(self.area_full(), self.chrome().top.h, &self.theme)
    }

    /// The address BitsOS knows in `text` (what `open_address` opens): its row's title and address.
    fn feld_known(&self, text: &str) -> Option<(String, String)> {
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        if let Some(e) = self.library.by_address(t).filter(|e| !e.exec.is_empty()) {
            return Some((e.name.clone(), e.address.clone()));
        }
        let addr = match address::resolve(t) {
            Open::Terminal => "programm/terminal".to_string(),
            Open::Sofa => address::SOFA.to_string(),
            Open::Office(o) if std::path::Path::new(address::OFFICE_LAUNCHER).is_file() => format!("programm/{}", o.word()),
            Open::Programme(_) => address::normalise(t),
            Open::System(_) => address::normalise(t),
            // A settings path names keys settingsd has (any while settingsd has not listed them).
            Open::Settings(p) if p.is_empty() || self.setting_keys.is_empty() || self.setting_keys.iter().any(|(k, _)| *k == p || k.starts_with(&format!("{p}."))) => {
                address::normalise(t)
            }
            _ => return None,
        };
        let name = self.library.by_address(&addr).map(|e| e.name.as_str());
        Some((feld::address_title(&addr, name), addr))
    }

    /// The rows for the text typed now (kept until it changes: the pointer moves far more often).
    pub fn feld_refresh(&mut self) {
        let text = self.feld.edit.text.clone();
        let hits: Vec<feld::Hit> = if text.trim().is_empty() {
            Vec::new()
        } else {
            self.library.matches(&text).iter().map(|e| (e.id.clone(), e.name.clone(), e.address.clone())).collect()
        };
        let name = self.assist_on().then(|| self.assist.name.clone());
        self.feld.rows = feld::rows(&text, self.feld_known(&text), &hits, name.as_deref());
        self.needs_frame = true;
    }

    /// The rows that fit the panel, each with its place; when the panel is short, programs are left out first, so
    /// the question's row stays.
    pub fn feld_placed(&self) -> (Geo, Vec<Row>, Vec<(Option<Rect>, Rect)>) {
        let g = self.feld_geo();
        let mut rows = self.feld.rows.clone();
        loop {
            let kinds: Vec<Kind> = rows.iter().map(|r| r.kind).collect();
            let placed = view::place(&g, &kinds, &self.theme);
            if placed.len() >= rows.len() {
                return (g, rows, placed);
            }
            match rows.iter().rposition(|r| r.kind == Kind::Program) {
                Some(k) => {
                    rows.remove(k);
                }
                None => {
                    rows.truncate(placed.len());
                    return (g, rows, placed);
                }
            }
        }
    }

    /// A key while the Feld is open; false for the modifiers (they reach the client as before, so neither the
    /// Win key of Win+Space nor a held Shift is left pressed in it).
    pub fn feld_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        if crate::library_ui::is_modifier(sym) {
            return false;
        }
        let n = self.feld_placed().1.len();
        let e = &mut self.feld.edit;
        let pick = e.pick.unwrap_or(0);
        let mut typed = true;
        match sym {
            Keysym::Escape => {
                self.feld_close("Esc");
                return true;
            }
            Keysym::Return | Keysym::KP_Enter => {
                let at = self.feld.pick(n);
                if let Some(i) = at {
                    self.feld_do(i);
                }
                return true;
            }
            Keysym::Down | Keysym::KP_Down | Keysym::Tab if n > 0 => (e.pick, typed) = (Some((pick + 1) % n), false),
            Keysym::Up | Keysym::KP_Up | Keysym::ISO_Left_Tab if n > 0 => (e.pick, typed) = (Some((pick + n - 1) % n), false),
            Keysym::BackSpace => e.backspace(),
            Keysym::Delete | Keysym::KP_Delete => e.delete(),
            Keysym::Left | Keysym::KP_Left | Keysym::Right | Keysym::KP_Right | Keysym::Home | Keysym::KP_Home | Keysym::End | Keysym::KP_End => {
                match sym {
                    Keysym::Left | Keysym::KP_Left => e.left(),
                    Keysym::Right | Keysym::KP_Right => e.right(),
                    Keysym::Home | Keysym::KP_Home => e.home(),
                    _ => e.end(),
                }
                typed = false;
            }
            _ => match sym.key_char() {
                Some(c) if !c.is_control() && !mods.ctrl && !mods.alt && !mods.logo => {
                    e.insert(c);
                    self.feld.remote |= self.input_origin != Origin::Physical;
                }
                _ => return true,
            },
        }
        if typed {
            self.feld_refresh();
        }
        self.needs_frame = true;
        true
    }

    /// Does row `i`: the Feld closes first, then the address opens, the program starts or the question goes.
    pub fn feld_do(&mut self, i: usize) {
        let (_, rows, _) = self.feld_placed();
        let Some(r) = rows.get(i).cloned() else { return };
        let (real, remote) = (self.input_origin == Origin::Physical, self.feld.remote);
        self.feld_close(r.kind.name());
        match r.kind {
            Kind::Address => {
                eprintln!("bitscomp: feld: opens {}", r.detail);
                self.open_address(&r.target, Where::Tab, None);
            }
            Kind::Program => {
                eprintln!("bitscomp: feld: starts {}", r.target);
                self.library_start(&r.target, Where::Tab);
                // The library is closed: its line about a start that failed is the desktop's notice.
                if !self.lib.open
                    && let Some(n) = self.lib.note.take()
                {
                    self.notice(n);
                }
            }
            Kind::Ask => self.feld_ask(&r.target, real, remote),
        }
    }

    /// The question goes to the sidebar's assistant: the sidebar opens (as its ring does), its input line gets the
    /// focus, and the text is submitted from there; a draft Georg left in that line stays his (the question is
    /// asked beside it, as `ctl assist ask` does). Refused (no provider, still answering ...): the assistant's own
    /// line says why, above its input, or as the notice where the sidebar is not on the screen.
    fn feld_ask(&mut self, text: &str, real: bool, remote: bool) {
        if !self.assist_on() {
            return;
        }
        let now = crate::pacer::now();
        if !self.theme.layout.sidebar {
            self.set_bar(crate::desk::SIDEBAR_KEY, true);
        }
        let injected = !real || remote;
        if injected {
            // What injected input asks may shift an open card: its Ja waits anew (as `ctl assist ask`).
            self.assist.touched(now);
        }
        // While a question is open, injected input does not steer Georg's keys into the sidebar (R8c).
        if real || self.assist.card.is_none() {
            self.assist_focus(real);
        }
        if let Some(c) = self.assist_client.as_ref().filter(|_| !self.assist.up) {
            c.poke();
        }
        let lines = if self.assist.edit.text.trim().is_empty() {
            let a = &mut self.assist;
            a.edit = address::Edit::default();
            for ch in text.chars() {
                if !a.insert(ch, injected) {
                    break;
                }
            }
            a.submit(now, injected)
        } else {
            self.assist.ask(text, now, injected)
        };
        let sent = !lines.is_empty();
        self.assist_sync(lines);
        eprintln!("bitscomp: feld: question to the assistant {}", if sent { "sent" } else { "not sent" });
        if !sent
            && self.assist_place().is_none()
            && let Some((n, _)) = self.assist.note.clone()
        {
            self.notice(n);
        }
    }

    fn feld_row_at(&self, x: f64, y: f64) -> Option<usize> {
        self.feld_placed().2.iter().position(|(_, r)| r.contains(x, y))
    }

    /// A press while the Feld is open (every press is its own, so is its release).
    pub fn feld_press(&mut self, code: u32) {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let g = self.feld_geo();
        if !g.panel.contains(x, y) {
            return self.feld_close("click outside");
        }
        if g.esc.contains(x, y) {
            return self.feld_close("Esc chip");
        }
        if code == crate::pointer::BTN_LEFT
            && let Some(i) = self.feld_row_at(x, y)
        {
            self.feld_do(i);
        }
    }

    pub fn feld_motion(&mut self) {
        let (x, y) = (self.pointer.x, self.pointer.y);
        let hover = self.feld_row_at(x, y);
        let esc = self.feld_geo().esc.contains(x, y);
        if hover != self.feld.hover || esc != self.feld.hover_esc {
            (self.feld.hover, self.feld.hover_esc) = (hover, esc);
            self.needs_frame = true;
        }
    }

    /// `ctl feld state`: open, the text's length (what Georg writes is not handed to every program that reaches the
    /// socket, the rule of `ctl assist`), the pick and every row.
    pub fn feld_state(&self) -> String {
        let (_, rows, _) = self.feld_placed();
        let pick = self.feld.pick(rows.len()).map_or("-".to_string(), |i| i.to_string());
        let e = &self.feld.edit;
        let text = format!("len={}", e.text.chars().count());
        let mut out = format!("feld open={} {text} cursor={} pick={pick} rows={} remote={}\n", self.feld.open, e.cursor, rows.len(), self.feld.remote);
        for (i, r) in rows.iter().enumerate() {
            out.push_str(&format!("row {i} kind={}{}\n", r.kind.name(), self.feld_row_words(r)));
        }
        out.push_str("end");
        out
    }

    /// A row's words for the control socket: the address or program it opens; a question's text only in the test
    /// build.
    fn feld_row_words(&self, r: &Row) -> String {
        match r.kind {
            Kind::Address | Kind::Program => format!(" title={:?} addr={}", r.title, r.detail),
            Kind::Ask => format!(" to={:?}", r.detail),
        }
    }

    /// `ctl hot`: the Feld bar (`-` where the preset has none) and, while open, the Esc chip and the rows.
    pub fn feld_hot(&self) -> String {
        let s = self.theme.scale;
        let mid = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        let bar = self.buehne().filter(|b| b.bottom.is_some()).map_or("-".to_string(), |b| mid(b.feld));
        let mut out = format!("feld at={bar} open={}\n", self.feld.open);
        if !self.feld.open {
            return out;
        }
        let (g, rows, placed) = self.feld_placed();
        out.push_str(&format!("feld esc at={}\n", mid(g.esc)));
        let pick = self.feld.pick(rows.len());
        for (i, (r, (_, at))) in rows.iter().zip(&placed).enumerate() {
            out.push_str(&format!("feld row {i} at={} kind={} pick={}{}\n", mid(*at), r.kind.name(), pick == Some(i), self.feld_row_words(r)));
        }
        out
    }
}

/// `ctl feld open|close|state|type TEXT`: `type` puts each character through the Feld's own key handling, as
/// injected input.
pub fn ctl(app: &mut App, words: &[&str], line: &str) -> Option<Result<String, String>> {
    if words.first() != Some(&"feld") {
        return None;
    }
    Some(match words.get(1).copied() {
        Some("open") => {
            app.feld_open("ctl");
            if app.feld.open { Ok(app.feld_state()) } else { Err(format!("feld: not opened: {}", app.feld_refused().unwrap_or("?"))) }
        }
        Some("close") => {
            app.feld_close("ctl");
            Ok(app.feld_state())
        }
        Some("state") | None => Ok(app.feld_state()),
        Some("type") if app.feld.open => {
            let text = line.split_once(" type ").map_or("", |(_, r)| r);
            app.input_origin = app.ctl_origin;
            for c in text.chars() {
                app.feld_key(Keysym::from_char(c), &ModifiersState::default());
            }
            Ok(app.feld_state())
        }
        Some("type") => Err("feld: not open".into()),
        _ => Err("usage: feld open|close|state|type TEXT".into()),
    })
}
