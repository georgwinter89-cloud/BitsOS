//! WY-5b: the settings page in bitscomp. A page is the window of a `bitscomp page ADDRESS` bitscomp started
//! (`page_client`, app id `bitscomp.einstellungen`); its content is bitscomp's own image (`page_draw`) at the
//! window's content rectangle, its clicks are spots (`Spot::Page`), its fields take the keys while edited, the wheel
//! scrolls it. Changes go to settingsd (`settings_write`); what settingsd then reports through the watch is what
//! the page shows.

use std::collections::HashMap;

use mcpreg::Json;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::Window;
use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::address::{Edit, Where};
use crate::deco::image_a;
use crate::desk::Spot;
use crate::header::px_rect;
use crate::layout::Rect;
use crate::page::{self, FAVOURITES, Inputs, Kind, Layout, Part, Row, SWATCHES, part_name};
use crate::page_draw::{FieldLook, PageLook};
use crate::render::CompElement;
use crate::settings_write::{Ask, Done, Req};
use crate::state::App;

pub const WHEEL_STEP: i32 = 48;

pub struct PageState {
    pub address: String,
    pub prefix: String,
    pub scroll: i32,
    /// Scroll to the prefix's first row at the first layout.
    pub jump: bool,
    pub edit: Option<(Part, Edit)>,
    pub msg: Option<(String, bool)>,
    /// WY-10: the open confirmation dialog (it takes the page's clicks and, while focused, the keys).
    pub confirm: Option<crate::page_confirm::Dialog>,
    image: Option<(PageLook, MemoryRenderBuffer)>,
}

impl PageState {
    pub fn new(address: &str) -> PageState {
        let a = crate::address::normalise(address);
        let prefix = page::prefix_of(a.strip_prefix("einstellungen").unwrap_or("").trim_start_matches('/'));
        PageState { address: a, jump: !prefix.is_empty(), prefix, scroll: 0, edit: None, msg: None, confirm: None, image: None }
    }
}

#[derive(Default)]
pub struct Pages {
    pub by_pid: HashMap<u32, PageState>,
    /// Changes sent to settingsd and not answered yet (all pages share the one writer).
    pub busy: usize,
}

/// The program that shows a page: this bitscomp (its path, so a restored session finds it again).
pub fn argv(address: &str) -> Vec<String> {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "bitscomp".into());
    vec![exe, "page".into(), address.into()]
}

fn is_page_argv(argv: &[&str]) -> Option<String> {
    let name = std::path::Path::new(argv.first()?).file_name()?.to_str()?;
    (name == "bitscomp" && argv.get(1) == Some(&"page")).then(|| argv.get(2).copied().unwrap_or("einstellungen").to_string())
}

fn label(app: &App, key: &str) -> String {
    let own = app.setting_keys.iter().find(|(k, _)| k == key).map(|(_, t)| t.clone()).filter(|t| !t.is_empty());
    own.or_else(|| app.page_model.rows.iter().find(|r| r.key == key).map(|r| r.label.clone())).unwrap_or_else(|| key.to_string())
}

/// What row `r` shows: settingsd's value, else its default (a malformed accent shows as the default).
pub(crate) fn value_of(app: &App, r: &Row) -> String {
    let v = app.settings.text(&r.key);
    match r.kind {
        Kind::Accent => v.and_then(page::colour).unwrap_or_else(|| r.default.clone()),
        _ => v.unwrap_or(&r.default).to_string(),
    }
}

impl App {
    /// Opens the settings page for `address` (`einstellungen/...`) like a program: a tab or a tile.
    pub fn open_page(&mut self, address: &str, w: Where, tile: Option<usize>) {
        eprintln!("bitscomp: settings page {address}");
        self.open_program(argv(address), w, tile);
    }

    /// Called for every program bitscomp starts: a page gets its state.
    pub fn page_spawned(&mut self, pid: u32, argv: &[&str]) {
        if let Some(a) = is_page_argv(argv) {
            self.pages.by_pid.insert(pid, PageState::new(&a));
        }
    }

    pub fn page_pid(&self, w: &Window) -> Option<u32> {
        if crate::ctl::names(w).0 != crate::page_client::APP_ID {
            return None;
        }
        let pid = u32::try_from(self.pid_of(w)).ok()?;
        self.pages.by_pid.contains_key(&pid).then_some(pid)
    }

    fn page_window(&self, pid: u32) -> Option<Window> {
        self.all_windows().into_iter().find(|w| self.page_pid(w) == Some(pid))
    }

    /// A window's address: a page's own, else its program's.
    pub fn address_of(&self, w: &Window) -> String {
        match self.page_pid(w).and_then(|p| self.pages.by_pid.get(&p)) {
            Some(s) => s.address.clone(),
            None => crate::address::of_window(&crate::ctl::names(w).0),
        }
    }

    pub fn page_layout(&self, pid: u32, content: Rect) -> Option<Layout> {
        let s = self.pages.by_pid.get(&pid)?;
        Some(page::layout(content, &self.page_model, &Inputs { favourites: self.desk.favourites.len(), scroll: s.scroll }))
    }

    /// The first layout jumps to the address's row; the scroll stays inside the page.
    fn page_prepare(&mut self, pid: u32, content: Rect) {
        let Some(lay) = self.page_layout(pid, content) else { return };
        let rows = &self.page_model.rows;
        let Some(s) = self.pages.by_pid.get_mut(&pid) else { return };
        if s.jump && content.h > 0 {
            s.jump = false;
            if let Some(to) = lay.scroll_to(&s.prefix, rows, s.scroll) {
                s.scroll = to;
            }
        }
        s.scroll = s.scroll.clamp(0, lay.max_scroll().max(0));
    }

    fn page_look(&self, pid: u32, content: Rect) -> Option<PageLook> {
        let s = self.pages.by_pid.get(&pid)?;
        let layout = self.page_layout(pid, content)?;
        let rows = self.page_model.rows.clone();
        let values = rows.iter().map(|r| value_of(self, r)).collect();
        let known = rows.iter().any(|r| r.key == s.prefix) || s.prefix == FAVOURITES;
        let hover = match self.desk.hover {
            Some(Spot::Page(p, part)) if p == pid && part != Part::Body => Some(part),
            _ => None,
        };
        Some(PageLook {
            layout,
            theme: self.theme_gen,
            address: s.address.clone(),
            rows,
            values,
            fav_text: label(self, FAVOURITES),
            favs: self.desk.favourites.clone(),
            target: known.then(|| s.prefix.clone()),
            hover,
            field: s.edit.as_ref().map(|(p, e)| FieldLook { part: *p, text: e.text.clone(), before: e.before().to_string() }),
            msg: s.msg.clone(),
            dialog: s.confirm.clone(),
            live: self.settings_live && self.settings_writer.is_some(),
        })
    }

    /// The page of `pid` drawn over its window's content `c` (logical pixels).
    pub fn page_elements(&mut self, pixman: &mut PixmanRenderer, pid: u32, c: Rect) -> Vec<CompElement> {
        if c.w <= 0 || c.h <= 0 {
            return Vec::new();
        }
        self.page_prepare(pid, c);
        let Some(look) = self.page_look(pid, c) else { return Vec::new() };
        let t = self.theme.clone();
        let s = self.pages.by_pid.get_mut(&pid).unwrap();
        if s.image.as_ref().is_none_or(|(l, _)| *l != look) {
            s.image = Some((look.clone(), crate::page_draw::draw(&look, &t).into_buffer()));
        }
        let buf = &s.image.as_ref().unwrap().1;
        let p = px_rect(c, t.scale);
        image_a(pixman, buf, c, (p.w, p.h), t.scale, 1.0).into_iter().collect()
    }

    /// The spot on page window `w` at (x, y), when the point is in its content.
    pub fn page_hit(&self, w: &Window, frame: Rect, x: f64, y: f64) -> Option<Spot> {
        let pid = self.page_pid(w)?;
        let lay = self.page_layout(pid, self.content_rect(frame))?;
        if let Some(d) = self.pages.by_pid.get(&pid).and_then(|s| s.confirm.as_ref()) {
            // Modal: only the dialog's buttons react; the rest of the page is its body.
            let hit = crate::page_confirm::parts(lay.content, d).into_iter().find(|(_, b)| b.contains(x, y)).map(|(p, _)| p);
            return lay.content.contains(x, y).then(|| Spot::Page(pid, hit.unwrap_or(Part::Body)));
        }
        lay.hit(x, y).map(|p| Spot::Page(pid, p))
    }

    /// A press on a page: focuses it; the right button opens the circle menu for it. True: the press is kept
    /// as a spot press (acting on release).
    pub fn page_press(&mut self, pid: u32, part: Part, right: bool) -> bool {
        if let Some(w) = self.page_window(pid) {
            if self.ws.focused() != Some(&w) {
                self.ws.focus_window(&w);
                self.layout_dirty = true;
            }
            if right {
                self.page_edit_end();
                self.open_menu(Some(w));
                return false;
            }
        }
        let keeps = self.pages.by_pid.get(&pid).and_then(|s| s.edit.as_ref()).is_some_and(|(p, _)| {
            *p == part || (*p == Part::FavField && part == Part::FavAdd)
        });
        if !keeps {
            self.page_edit_end();
        }
        true
    }

    /// A new row list (each settingsd list): an open row field follows its key, so Enter never writes the
    /// typed text into the key that took its old place; a key that is gone or no longer typed ends the edit.
    pub fn page_model_set(&mut self, new: page::Model) {
        let mut ended = false;
        for s in self.pages.by_pid.values_mut() {
            let Some((Part::Field(r), _)) = s.edit.as_ref() else { continue };
            match (crate::page_schema::field_row(&self.page_model, &new, *r), s.edit.as_mut()) {
                (Some(n), Some((part, _))) => *part = Part::Field(n),
                _ => ended |= s.edit.take().is_some(),
            }
        }
        if ended {
            eprintln!("bitscomp: page: settings list changed under an open field; edit ended, nothing written");
            self.desk.held = None;
        }
        let keys: Vec<String> = new.rows.iter().map(|r| r.key.clone()).collect();
        self.page_dialogs_drop(Some(&keys), "die Einstellung gibt es nicht mehr");
        self.page_model = new;
        self.needs_frame = true;
    }

    pub fn page_edit_end(&mut self) {
        let mut any = false;
        for s in self.pages.by_pid.values_mut() {
            any |= s.edit.take().is_some();
        }
        if any {
            self.desk.held = None;
            self.needs_frame = true;
        }
    }

    /// The page whose field takes the keys: the focused window, a page, with a field being edited.
    pub fn page_edit_pid(&self) -> Option<u32> {
        let pid = self.page_pid(self.ws.focused()?)?;
        self.pages.by_pid.get(&pid)?.edit.is_some().then_some(pid)
    }

    pub(crate) fn page_say(&mut self, pid: u32, text: impl Into<String>, error: bool) {
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.msg = Some((text.into(), error));
            self.needs_frame = true;
        }
    }

    pub(crate) fn page_send(&mut self, pid: u32, r: Req) {
        if !self.settings_live {
            return self.page_say(pid, "Der Einstellungsdienst antwortet nicht: nichts geändert.", true);
        }
        let sent = self.settings_writer.as_ref().is_some_and(|w| w.send(pid, r.clone()));
        if sent {
            eprintln!("bitscomp: settings page asks settingsd: {}", crate::settings_write::request_line(&r));
            self.pages.busy += 1;
            if self.pages.busy > 1 {
                // The one before is still on its way: the click is kept, and the page says it waits.
                eprintln!("bitscomp: settings page: {} changes wait for settingsd", self.pages.busy);
                self.page_say(pid, format!("Wartet auf den Einstellungsdienst ({} offen) …", self.pages.busy), false);
            }
        } else {
            self.page_say(pid, "Änderungen sind gerade nicht möglich.", true);
        }
    }

    /// Every change of the page passes here: a confirm key opens the dialog instead (WY-10).
    fn page_set(&mut self, pid: u32, key: &str, value: Json) {
        if self.page_model.rows.iter().any(|r| r.key == key && r.confirm) {
            let req = Req::Set { key: key.into(), value: value.clone(), confirmed: true };
            return self.page_ask(pid, Ask { req, key: key.into(), to: value });
        }
        self.page_send(pid, Req::set(key, value));
    }

    fn page_set_favs(&mut self, pid: u32, v: Vec<String>) {
        self.page_set(pid, FAVOURITES, Json::str(&v.join(", ")));
    }

    /// settingsd answered a change of the page `d.pid`.
    pub fn page_done(&mut self, d: Done) {
        // WY-5c: the answer to a fold of a side bar (no page asked).
        if d.pid == crate::desk::DESK_PID {
            if !d.ok {
                self.fold_refused(d.key.as_deref().unwrap_or("?"));
            }
            return;
        }
        self.pages.busy = self.pages.busy.saturating_sub(1);
        // settingsd wants a confirmation the page did not know of (an undo, or a row list from before): ask.
        if let Some(a) = d.ask.filter(|a| !a.key.is_empty()) {
            return self.page_ask(d.pid, a);
        }
        let text = match &d.key {
            Some(k) => {
                let short = self.page_model.rows.iter().find(|r| r.key == *k).map(|r| r.short.clone()).unwrap_or_else(|| label(self, k));
                format!("{short}: {}", d.text)
            }
            None => d.text.clone(),
        };
        self.page_say(d.pid, text, !d.ok);
    }

    pub fn page_act(&mut self, pid: u32, part: Part) {
        let v = &self.settings;
        // A part of a row that went away with a new registry list does nothing.
        let row = |r: usize| self.page_model.rows.get(r).cloned();
        match part {
            Part::Undo => self.page_send(pid, Req::UNDO),
            Part::DialogCancel => self.page_dialog_close(pid, false),
            Part::DialogOk => self.page_dialog_close(pid, true),
            Part::Choice(r, o) => {
                if let Some(Row { key, kind: Kind::Choice(opts), .. }) = row(r) {
                    if let Some((value, _)) = opts.get(o) {
                        self.page_set(pid, &key, Json::str(value));
                    }
                }
            }
            Part::Swatch(i) => {
                if let Some(s) = SWATCHES.get(i) {
                    self.page_set(pid, "theme.accent", Json::str(s));
                }
            }
            Part::Word(r) => {
                if let Some(Row { key, kind: Kind::Number { word: Some((w, _)), .. }, .. }) = row(r) {
                    self.page_set(pid, &key, Json::str(&w));
                }
            }
            Part::Less(r) | Part::More(r) => {
                if let Some(Row { key, kind: Kind::Number { min, max, step, word, .. }, default, .. }) = row(r) {
                    // The word (`alles`) counts as the top of the range: − leaves it, + stays.
                    let on_word = |t: &str| word.as_ref().is_some_and(|(w, _)| w == t);
                    let now = v.int(&key).or_else(|| default.parse().ok()).unwrap_or(min);
                    let now = if v.text(&key).map_or(on_word(&default), on_word) { max } else { now };
                    let next = page::number_step(now, matches!(part, Part::More(_)), min, max, step);
                    if next != now {
                        self.page_set(pid, &key, Json::Num(next as f64));
                    }
                }
            }
            Part::Field(r) => {
                if let Some(r) = row(r).filter(|r| matches!(r.kind, Kind::Number { .. } | Kind::Field { .. })) {
                    let now = value_of(self, &r);
                    self.page_edit_begin(pid, part, now);
                }
            }
            Part::Switch(r) => {
                if let Some(Row { key, kind: Kind::Switch, default, .. }) = row(r) {
                    let now = v.flag(&key).unwrap_or(default == "true");
                    self.page_set(pid, &key, Json::Bool(!now));
                }
            }
            Part::FavUp(i) => {
                let n = page::favs_up(&self.desk.favourites, i);
                self.page_set_favs(pid, n);
            }
            Part::FavRemove(i) => {
                let n = page::favs_without(&self.desk.favourites, i);
                self.page_set_favs(pid, n);
            }
            Part::Hex => {
                let now = v.text("theme.accent").and_then(page::colour).unwrap_or_else(|| crate::theme::ACCENT.into());
                self.page_edit_begin(pid, Part::Hex, now);
            }
            Part::FavField => self.page_edit_begin(pid, Part::FavField, String::new()),
            Part::FavAdd => {
                let editing = self.pages.by_pid.get(&pid).and_then(|s| s.edit.as_ref()).is_some_and(|(p, _)| *p == Part::FavField);
                if editing {
                    self.page_commit(pid);
                } else {
                    self.page_edit_begin(pid, Part::FavField, String::new());
                }
            }
            Part::Body => {}
        }
    }

    fn page_edit_begin(&mut self, pid: u32, part: Part, text: String) {
        self.address_end();
        self.page_edit_end();
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            let cursor = text.chars().count();
            s.edit = Some((part, Edit { text, cursor, pick: None, tile: None }));
            self.needs_frame = true;
        }
    }

    /// Enter in a field: the colour or the new favourite goes to settingsd.
    fn page_commit(&mut self, pid: u32) {
        let Some((part, e)) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.edit.take()) else { return };
        self.desk.held = None;
        self.needs_frame = true;
        match part {
            Part::Hex => match page::colour(&e.text) {
                Some(c) => self.page_set(pid, "theme.accent", Json::str(&c)),
                None => self.page_say(pid, format!("Keine gültige Farbe: „{}“ (Form #rrggbb)", e.text.trim()), true),
            },
            Part::FavField => match page::favs_with(&self.desk.favourites, &e.text) {
                Some(n) => self.page_set_favs(pid, n),
                None if e.text.trim().is_empty() => {}
                None => self.page_say(pid, format!("„{}“ ist schon ein Favorit.", e.text.trim()), true),
            },
            Part::Field(r) => self.page_commit_field(pid, r, &e.text),
            _ => {}
        }
    }

    /// Enter in row `r`'s field: the value goes to settingsd when it fits the row's type (settingsd checks again).
    fn page_commit_field(&mut self, pid: u32, r: usize, text: &str) {
        let Some(row) = self.page_model.rows.get(r).cloned() else { return };
        let value = match &row.kind {
            Kind::Number { word: Some((w, name)), .. }
                if text.trim().eq_ignore_ascii_case(w) || text.trim().eq_ignore_ascii_case(name) =>
            {
                Ok(Json::str(w))
            }
            Kind::Number { min, max, unit, word, .. } => page::number(text, *min, *max, unit)
                .map(|n| Json::Num(n as f64))
                .map_err(|e| match word {
                    Some((_, name)) => format!("{e} oder „{name}“"),
                    None => e,
                }),
            Kind::Field { colour: true, .. } => {
                page::colour(text).map(|c| Json::str(&c)).ok_or_else(|| format!("Keine gültige Farbe: „{}“ (Form #rrggbb)", text.trim()))
            }
            Kind::Field { colour: false, max } if text.len() > *max => Err(format!("Zu lang: höchstens {max} Bytes")),
            Kind::Field { colour: false, .. } => Ok(Json::str(text.trim())),
            _ => return,
        };
        match value {
            Ok(v) => self.page_set(pid, &row.key, v),
            Err(why) => self.page_say(pid, format!("{}: {why}", row.short), true),
        }
    }

    /// A key for the page's field being edited (every key stays with bitscomp then).
    pub fn page_key(&mut self, pid: u32, sym: Keysym, mods: &ModifiersState) {
        match sym {
            Keysym::Escape => return self.page_edit_end(),
            Keysym::Return | Keysym::KP_Enter => return self.page_commit(pid),
            _ => {}
        }
        let Some((_, e)) = self.pages.by_pid.get_mut(&pid).and_then(|s| s.edit.as_mut()) else { return };
        match sym {
            Keysym::BackSpace => e.backspace(),
            Keysym::Delete | Keysym::KP_Delete => e.delete(),
            Keysym::Left | Keysym::KP_Left => e.left(),
            Keysym::Right | Keysym::KP_Right => e.right(),
            Keysym::Home | Keysym::KP_Home => e.home(),
            Keysym::End | Keysym::KP_End => e.end(),
            _ => match sym.key_char() {
                Some(c) if !c.is_control() && !mods.ctrl && !mods.logo && e.text.chars().count() < 4096 => e.insert(c),
                _ => return,
            },
        }
        self.needs_frame = true;
    }

    pub fn page_scroll(&mut self, pid: u32, steps: f64) {
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            let before = s.scroll;
            s.scroll = (s.scroll + (steps * WHEEL_STEP as f64).round() as i32).max(0);
            // The upper end is clamped at the next layout (the content size is known there).
            if s.scroll != before {
                self.needs_frame = true;
            }
        }
    }

    /// Pages of programs that ended are forgotten.
    pub fn pages_reap(&mut self) {
        let alive = &self.spawned;
        self.pages.by_pid.retain(|p, _| alive.contains_key(p));
    }

    /// `ctl hot` lines of every visible page's controls: `page pid=N NAME at=X,Y`.
    pub fn page_hot(&self) -> String {
        let s = self.theme.scale;
        let mut out = String::new();
        for p in self.placements().into_iter().filter(|p| p.visible) {
            let Some(pid) = self.page_pid(&p.win) else { continue };
            let Some(lay) = self.page_layout(pid, self.content_rect(p.frame)) else { continue };
            let dialog = self.pages.by_pid.get(&pid).and_then(|s| s.confirm.as_ref());
            let parts = dialog.map_or_else(|| lay.parts.clone(), |d| crate::page_confirm::parts(lay.content, d));
            for (part, b) in &parts {
                let (x, y) = (((b.x + b.w / 2) as f64 * s).round(), ((b.y + b.h / 2) as f64 * s).round());
                out.push_str(&format!("page pid={pid} {} at={x},{y}\n", part_name(&self.page_model.rows, *part)));
            }
        }
        out
    }

    pub fn page_clear_msgs(&mut self) {
        for s in self.pages.by_pid.values_mut() {
            s.msg = None;
        }
        self.page_dialogs_drop(None, "");
        self.needs_frame = true;
    }

    /// `ctl page`: each page's state.
    pub fn page_report(&self) -> String {
        let mut v: Vec<_> = self.pages.by_pid.iter().collect();
        v.sort_by_key(|(p, _)| **p);
        let mut out = String::new();
        for (pid, s) in v {
            let edit = s.edit.as_ref().map_or("none".to_string(), |(p, e)| format!("{}:{:?}", part_name(&self.page_model.rows, *p), e.text));
            let dialog = s.confirm.as_ref().map_or("none".to_string(), |d| {
                format!("{}:{}:focus={}", if matches!(d.ask.req, Req::Undo { .. }) { "undo" } else { "set" }, d.ask.key, if d.on_ok { "ok" } else { "cancel" })
            });
            let msg = s.msg.as_ref().map_or("none".to_string(), |(m, err)| format!("{}{m:?}", if *err { "error:" } else { "" }));
            out.push_str(&format!(
                "page pid={pid} address={} prefix={} scroll={} edit={edit} dialog={dialog} msg={msg} live={} busy={}\n",
                s.address,
                if s.prefix.is_empty() { "-" } else { &s.prefix },
                s.scroll,
                self.settings_live && self.settings_writer.is_some(),
                self.pages.busy
            ));
        }
        out.push_str("end");
        out
    }
}
