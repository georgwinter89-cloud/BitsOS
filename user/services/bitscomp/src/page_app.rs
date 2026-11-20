//! WY-5b: the settings page in bitscomp. A page is the window of a `bitscomp page ADDRESS` bitscomp started
//! (`page_client`, app id `bitscomp.einstellungen`); its content is bitscomp's own image (`page_draw`) at the
//! window's content rectangle, its clicks are spots (`Spot::Page`), its fields take the keys while edited, the wheel
//! scrolls it. Changes go to settingsd (`settings_write`); what settingsd then reports through the watch is what
//! the page shows.
//! PK-7a: this is the generic part of bitscomp's page windows (the window, scroll, hit, the edited field, the
//! confirmation dialog, the cached image); `PageKind` switches what a page shows: the settings (this file, page,
//! page_draw) or the "Programme" page (programme_app and its files).

use std::collections::HashMap;

use mcpreg::Json;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use crate::win::Window;
use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::address::{Edit, Where};
use crate::deco::image_a;
use crate::desk::Spot;
use crate::header::px_rect;
use crate::layout::Rect;
use crate::page::{self, FAVOURITES, Inputs, Kind, Layout, Part, Row, SWATCHES, part_name};
use crate::page_draw::{FieldLook, PageLook};
use crate::page_kind::PageKind;
use crate::render::{CompElement, CompRenderer};
use crate::settings_write::{Ask, Done, Req};
use crate::state::App;

pub const WHEEL_STEP: i32 = 48;

pub struct PageState {
    pub kind: PageKind,
    pub address: String,
    pub prefix: String,
    pub scroll: i32,
    /// Scroll to the prefix's first row at the first layout.
    pub jump: bool,
    pub edit: Option<(Part, Edit)>,
    pub msg: Option<(String, bool)>,
    /// WY-10: the open confirmation dialog (it takes the page's clicks and, while focused, the keys).
    pub confirm: Option<crate::page_confirm::Dialog>,
    /// PK-7a: the programme page's tab and program.
    pub view: crate::programme_view::View,
    /// SYS-A: the System window's tab, range, choice and search.
    pub sys: crate::sys_view::View,
    /// WY-SET: the settings window's tab (`tab_given`: named by the address or a click), its search, the list of
    /// changed keys, what it shows now (`page_tabs_app`), the keys a consolidated row waits to write, and the
    /// name settingsd's next answers are said under (a consolidated row's, for its keys' answers).
    pub tab: usize,
    pub tab_given: bool,
    /// The content size the last jump was computed for.
    pub jumped: Option<(i32, i32)>,
    pub query: String,
    pub changed_only: bool,
    pub shown: crate::page_view::View,
    pub many: Option<(String, Vec<(String, Json)>)>,
    pub answers_as: Option<(String, usize)>,
    image: Option<(Look, MemoryRenderBuffer)>,
}

/// What a page's cached image was drawn from.
#[derive(Clone, Debug, PartialEq)]
enum Look {
    /// The settings page and the page "KI" (the same drawing).
    Settings(PageLook),
    Programme(Box<crate::programme_draw::ProgLook>),
    System(Box<crate::sys_draw::SysLook>),
}

impl PageState {
    pub fn new(address: &str) -> PageState {
        let a = crate::address::normalise(address);
        let kind = PageKind::of_address(&a);
        let base = |address: String, prefix: String| PageState {
            kind,
            address,
            jump: !prefix.is_empty(),
            prefix,
            scroll: 0,
            edit: None,
            msg: None,
            confirm: None,
            view: Default::default(),
            sys: Default::default(),
            tab: 0,
            tab_given: false,
            jumped: None,
            query: String::new(),
            changed_only: false,
            shown: Default::default(),
            many: None,
            answers_as: None,
            image: None,
        };
        if kind == PageKind::Programme {
            let view = crate::programme_view::View::of_address(&a);
            return PageState { view, ..base(String::new(), String::new()) }.with_address();
        }
        if kind == PageKind::System {
            let sys = crate::sys_view::View::of_address(&a);
            let address = crate::sys_view::address(sys.tab);
            return PageState { sys, ..base(address, String::new()) };
        }
        if kind == PageKind::Ki {
            return base(a.clone(), PageKind::ki_prefix(&a));
        }
        let path = a.strip_prefix("einstellungen").unwrap_or("").trim_start_matches('/');
        let tab = crate::page_tabs::tab_of_path(path);
        PageState { tab: tab.unwrap_or(0), tab_given: tab.is_some(), ..base(a.clone(), page::prefix_of(path)) }
    }

    fn with_address(mut self) -> PageState {
        self.address = self.view.address();
        self
    }
}

#[derive(Default)]
pub struct Pages {
    pub by_pid: HashMap<u32, PageState>,
    /// WY-SET: the cards whose "Mehr" Georg opened (all settings windows, while bitscomp runs).
    pub open: std::collections::BTreeSet<String>,
    /// Changes sent to settingsd and not answered yet (all pages share the one writer).
    pub busy: usize,
    /// CL-5: the click or key being handled on a page is Georg's own: a click whose press, release and last
    /// pointer move all came from a real device, a key from a real keyboard (`ki_ask`).
    pub real: bool,
    /// GPY-PASTE: the paste into a field on its way (`page_paste`).
    pub paste: crate::page_paste::PagePaste,
}

/// The program that shows a page: this bitscomp (its path, so a restored session finds it again).
pub fn argv(address: &str) -> Vec<String> {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "bitscomp".into());
    vec![exe, "page".into(), address.into()]
}

pub(crate) fn is_page_argv(argv: &[&str]) -> Option<String> {
    let name = std::path::Path::new(argv.first()?).file_name()?.to_str()?;
    (name == "bitscomp" && argv.get(1) == Some(&"page")).then(|| argv.get(2).copied().unwrap_or("einstellungen").to_string())
}

fn label(app: &App, key: &str) -> String {
    let own = app.setting_keys.iter().find(|(k, _)| k == key).map(|(_, t)| t.clone()).filter(|t| !t.is_empty());
    own.or_else(|| app.page_model.rows.iter().find(|r| r.key == key).map(|r| r.label.clone())).unwrap_or_else(|| key.to_string())
}

/// What row `r` shows: settingsd's value, else its default (a malformed accent shows as the default).
pub(crate) fn value_of(app: &App, r: &Row) -> String {
    if crate::leistung::is_own(&r.key) {
        return crate::leistung::value(&app.leistung.data, &r.key, crate::leistung::generation());
    }
    // WY-SET: the tabs' own rows (consolidations, the network's state).
    if let Kind::Subs(subs) = &r.kind {
        return subs.iter().map(|s| app.page_sub_value(&s.key)).collect::<Vec<_>>().join(";");
    }
    if r.key.starts_with(crate::page_tabs::VIRTUAL) {
        let get = |k: &str| app.page_model.rows.iter().any(|x| x.key == k).then(|| app.page_sub_value(k));
        if let Some(v) = crate::page_join::value(&r.key, &get) {
            return v;
        }
        return match r.key.as_str() {
            crate::page_join::VERBINDUNG => app.status.blocks.iter().find(|b| b.label == "Netz").map_or("–".into(), |b| b.value.clone()),
            crate::page_join::FRAG => "Win+F1".into(),
            _ => String::new(),
        };
    }
    let v = app.settings.text(&r.key);
    match r.kind {
        Kind::Accent => v.and_then(page::colour).unwrap_or_else(|| r.default.clone()),
        _ => v.unwrap_or(&r.default).to_string(),
    }
}

impl App {
    /// Opens the settings page for `address` (`einstellungen/...`) like a program: a tab or a tile.
    pub fn open_page(&mut self, address: &str, w: Where, tile: Option<usize>) {
        let kind = match PageKind::of_address(address) {
            PageKind::Programme => "programme",
            PageKind::Ki => "ki",
            PageKind::WerHatWas => "werhatwas",
            PageKind::Update => "aktualisieren",
            PageKind::System => "system",
            PageKind::Settings => "settings",
        };
        eprintln!("bitscomp: {kind} page {address}");
        self.open_program(argv(address), w, tile);
    }

    /// Called for every program bitscomp starts: a page gets its state.
    pub fn page_spawned(&mut self, pid: u32, argv: &[&str]) {
        if let Some(a) = is_page_argv(argv) {
            let s = PageState::new(&a);
            let kind = s.kind;
            self.pages.by_pid.insert(pid, s);
            self.page_tab_resolve(pid);
            self.page_view_refresh(pid);
            match kind {
                PageKind::Programme => self.prog_opened(pid, &a),
                PageKind::Ki => {
                    self.ki_helpers_scan();
                    self.ki_refresh();
                }
                PageKind::WerHatWas => {
                    self.wer.only = crate::sys_wer::program_of(&a);
                    self.wer_opened();
                }
                PageKind::System => self.sys_opened(),
                PageKind::Update => self.upd_opened(),
                PageKind::Settings => {}
            }
        }
    }

    pub fn page_pid(&self, w: &Window) -> Option<u32> {
        let kind = PageKind::of_app_id(&crate::ctl::names(w).0)?;
        let pid = u32::try_from(self.pid_of(w)).ok()?;
        self.pages.by_pid.get(&pid).is_some_and(|s| s.kind.app_id() == kind.app_id()).then_some(pid)
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

    pub(crate) fn page_kind(&self, pid: u32) -> PageKind {
        self.pages.by_pid.get(&pid).map_or(PageKind::Settings, |s| s.kind)
    }

    /// The rows settings window `pid` shows: its tab's view (WY-SET), or the page "KI"'s own.
    pub(crate) fn page_rows_of(&self, pid: u32) -> &page::Model {
        static EMPTY: std::sync::LazyLock<page::Model> = std::sync::LazyLock::new(page::Model::default);
        match self.pages.by_pid.get(&pid) {
            Some(s) if s.kind == PageKind::Ki => &self.ki.built.model,
            Some(s) if s.kind == PageKind::WerHatWas => &self.wer.built.model,
            Some(s) if s.kind == PageKind::Update => &self.upd.built.model,
            Some(s) => &s.shown.model,
            None => &EMPTY,
        }
    }

    pub fn page_layout(&self, pid: u32, content: Rect) -> Option<Layout> {
        let s = self.pages.by_pid.get(&pid).filter(|s| !matches!(s.kind, PageKind::Programme | PageKind::System))?;
        if s.kind == PageKind::Settings {
            return self.page_layout_tabs(pid, content);
        }
        Some(page::layout(content, self.page_rows_of(pid), &Inputs { favourites: self.desk.favourites.len(), scroll: s.scroll }))
    }

    /// The first layout jumps to the address's row; the scroll stays inside the page.
    fn page_prepare(&mut self, pid: u32, content: Rect) {
        if self.page_kind(pid) == PageKind::Programme {
            return self.prog_prepare(pid, content);
        }
        if self.page_kind(pid) == PageKind::System {
            return self.sys_prepare(pid, content);
        }
        let Some(lay) = self.page_layout(pid, content) else { return };
        let rows = self.page_rows_of(pid).rows.clone();
        let rows = &rows;
        let Some(s) = self.pages.by_pid.get_mut(&pid) else { return };
        // WY-SET: a settings tab's jump is computed again while the window still finds its size (it stays armed
        // until Georg scrolls or clicks), always from the page's top: the same address shows the same picture.
        if s.jump && content.h > 0 && s.kind == PageKind::Settings {
            if s.jumped != Some((content.w, content.h)) {
                s.jumped = Some((content.w, content.h));
                if let Some(r) = lay.row_at(&s.prefix, rows) {
                    let top = r.y + s.scroll - content.y;
                    let (from, h) = (page::HEAD_AREA + 16, content.h - page::HEAD_AREA - 16);
                    s.scroll = if top >= from && top + r.h <= from + h { 0 } else { (top - from - 64).max(0) };
                }
            }
        } else if s.jump && content.h > 0 {
            s.jump = false;
            if let Some(to) = lay.scroll_to(&s.prefix, rows, s.scroll) {
                // CL-5: the page "KI" jumps to a provider's section: its head stays in view above the first row.
                // DB4: so does the page "Was BitsOS über dich gelernt hat" (its head names the page).
                let head = s.kind == PageKind::Ki || s.prefix == crate::gelernt::GROUP || s.prefix == crate::db_settings::GROUP;
                // WY-SET: on the tabs the card's title stays in view above the row.
                s.scroll = if head && to != s.scroll { (to - 28).max(0) } else { to };
            }
        }
        s.scroll = s.scroll.clamp(0, lay.max_scroll().max(0));
    }

    pub(crate) fn page_look(&self, pid: u32, content: Rect) -> Option<PageLook> {
        let s = self.pages.by_pid.get(&pid)?;
        let layout = self.page_layout(pid, content)?;
        let ki = s.kind == PageKind::Ki;
        let wer = s.kind == PageKind::WerHatWas;
        let upd = s.kind == PageKind::Update;
        let rows = self.page_rows_of(pid).rows.clone();
        let now = if wer { Some(self.wer_now()) } else if upd { Some(self.upd.built.clone()) } else { None };
        let tabs = s.kind == PageKind::Settings;
        let values = if ki { self.ki.built.values.clone() } else if let Some(b) = &now { b.values.clone() } else { rows.iter().map(|r| value_of(self, r)).collect() };
        let known = rows.iter().any(|r| r.key == s.prefix) || s.prefix == FAVOURITES;
        let hover = match self.desk.hover {
            Some(Spot::Page(p, part)) if p == pid && part != Part::Body => Some(part),
            _ => None,
        };
        Some(PageLook {
            layout,
            theme: self.theme_gen,
            // WY-SET: under a tab's title the line that says what it holds (the address is in the address bar).
            address: if tabs { s.shown.sub.clone() } else { s.address.clone() },
            fav_text: label(self, FAVOURITES),
            favs: self.desk.favourites.clone(),
            target: known.then(|| s.prefix.clone()),
            hover,
            // A key being typed never reaches the look (the image is kept): the field only knows it is edited.
            field: s.edit.as_ref().map(|(p, e)| match p {
                Part::Field(r) if matches!(rows.get(*r).map(|r| &r.kind), Some(Kind::Secret { .. })) => FieldLook { part: *p, text: String::new(), before: String::new() },
                _ => FieldLook { part: *p, text: e.text.clone(), before: e.before().to_string() },
            }),
            msg: s.msg.clone(),
            dialog: s.confirm.clone(),
            live: self.settings_live && self.settings_writer.is_some(),
            title: if ki { crate::ki::TITLE.into() } else if wer { crate::werhatwas::TITLE.into() } else if upd { crate::update_page::TITLE.into() } else { s.shown.title.clone() },
            hint: if ki { crate::ki::HINT } else if wer { crate::werhatwas::HINT } else if upd { crate::update_page::HINT } else { crate::page_draw::HINT }.into(),
            subs: if ki {
                self.ki.built.subs.clone()
            } else if let Some(b) = now {
                b.subs
            } else {
                let mut subs = self.leistung_subs(&rows);
                self.gelernt_subs(&rows, &mut subs);
                let providers = self.page_providers();
                subs.resize(rows.len(), None);
                for (i, r) in rows.iter().enumerate() {
                    if let Some(line) = crate::page_join::sub_line(&r.key, &values[i], &providers) {
                        subs[i] = Some((line, crate::page_draw::Tone::Plain));
                    } else if let Some(Some(line)) = s.shown.lines.get(i).filter(|_| subs[i].is_none()) {
                        // WY-SET: no key addresses under the rows, settingsd's words instead.
                        subs[i] = Some((line.clone(), crate::page_draw::Tone::Plain));
                    }
                }
                subs
            },
            flags: if tabs { s.shown.model.flags.clone() } else { Vec::new() },
            rows,
            values,
        })
    }

    /// The page of `pid` drawn over its window's content `c` (logical pixels).
    pub fn page_elements<R: CompRenderer>(&mut self, renderer: &mut R, pid: u32, c: Rect) -> Vec<CompElement<R>> {
        if c.w <= 0 || c.h <= 0 {
            return Vec::new();
        }
        self.page_prepare(pid, c);
        let look = match self.page_kind(pid) {
            PageKind::Settings | PageKind::Ki | PageKind::WerHatWas | PageKind::Update => self.page_look(pid, c).map(Look::Settings),
            PageKind::Programme => self.prog_look(pid, c).map(|l| Look::Programme(Box::new(l))),
            PageKind::System => self.sys_look(pid, c).map(|l| Look::System(Box::new(l))),
        };
        let Some(look) = look else { return Vec::new() };
        let t = self.theme.clone();
        let s = self.pages.by_pid.get_mut(&pid).unwrap();
        if s.image.as_ref().is_none_or(|(l, _)| *l != look) {
            let canvas = match &look {
                Look::Settings(l) => crate::page_draw::draw(l, &t),
                Look::Programme(l) => crate::programme_draw::draw(l, &t),
                Look::System(l) => crate::sys_draw::draw_cached(l, &t, self.sys.frames.entry(pid).or_default()),
            };
            s.image = Some((look, canvas.into_buffer()));
        }
        let buf = &s.image.as_ref().unwrap().1;
        let p = px_rect(c, t.scale);
        image_a(renderer, buf, c, (p.w, p.h), t.scale, 1.0).into_iter().collect()
    }

    /// The spot on page window `w` at (x, y), when the point is in its content.
    pub fn page_hit(&self, w: &Window, frame: Rect, x: f64, y: f64) -> Option<Spot> {
        let pid = self.page_pid(w)?;
        let content = self.content_rect(frame);
        if let Some(d) = self.pages.by_pid.get(&pid).and_then(|s| s.confirm.as_ref()) {
            // Modal: only the dialog's buttons react; the rest of the page is its body.
            let hit = crate::page_confirm::parts(content, d).into_iter().find(|(_, b)| b.contains(x, y)).map(|(p, _)| p);
            return content.contains(x, y).then(|| Spot::Page(pid, hit.unwrap_or(Part::Body)));
        }
        let hit = match self.page_kind(pid) {
            PageKind::Settings | PageKind::Ki | PageKind::WerHatWas | PageKind::Update => self.page_layout(pid, content)?.hit(x, y),
            PageKind::Programme => self.prog_layout(pid, content)?.1.hit(x, y),
            PageKind::System => {
                let (_, l) = self.sys_layout(pid, content)?;
                if !content.contains(x, y) {
                    return None;
                }
                Some(l.hit(x, y).map_or(Part::Body, Part::Sys))
            }
        };
        hit.map(|p| Spot::Page(pid, p))
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
                // PK-7a: a library tile on the programme page has the library's own menu.
                if let Part::Prog(p) = part
                    && self.prog_right(pid, p)
                {
                    return false;
                }
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
        let keys: Vec<String> = new.rows.iter().map(|r| r.key.clone()).collect();
        self.page_dialogs_drop(Some(&keys), "die Einstellung gibt es nicht mehr");
        self.page_model = new;
        // WY-SET: each settings window's view follows (an open field follows its key there).
        self.page_views_refresh();
        self.needs_frame = true;
        // CL-5: the page "KI" takes its choices and limits from the registry's rows.
        self.ki_refresh();
    }

    pub fn page_edit_end(&mut self) {
        let mut any = false;
        // GPY-PASTE: a paste on its way is for the field that ends here (its text is wiped when it comes).
        self.pages.paste.open = None;
        for s in self.pages.by_pid.values_mut() {
            if let Some((_, e)) = s.edit.take() {
                any = true;
                // CL-5: what was typed on the page "KI" may have been a key; on "Aktualisieren" the token.
                if matches!(s.kind, PageKind::Ki | PageKind::Update) {
                    crate::settings_write::wipe(e.text);
                }
            }
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
            eprintln!("bitscomp: settings page asks settingsd: {}", crate::settings_write::logged_line(&r));
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
    pub(crate) fn page_set(&mut self, pid: u32, key: &str, value: Json) {
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
            if self.assist_set_done(&d) {
                return;
            }
            if !d.ok {
                self.fold_refused(d.key.as_deref().unwrap_or("?"));
            }
            return;
        }
        if d.pid == crate::assist_menu_app::PID {
            return self.assist_menu_done(d);
        }
        // GM-6c: the sofa mode's Einstellungen.
        if d.pid == crate::sofa_settings::SOFA_PID {
            return self.sofa_set_done(d);
        }
        self.pages.busy = self.pages.busy.saturating_sub(1);
        // settingsd wants a confirmation the page did not know of (an undo, or a row list from before): ask.
        if let Some(a) = d.ask.clone().filter(|a| !a.key.is_empty()) {
            return self.page_ask(d.pid, a);
        }
        // CL-5: by who asked, not by the page that is left: the page "KI" may have closed while its add or
        // removal was on its way.
        if self.page_kind(d.pid) == PageKind::Ki || (!self.pages.by_pid.contains_key(&d.pid) && self.ki_waits(d.pid)) {
            return self.ki_done(d);
        }
        // WY-SET: the answers to a consolidated row's keys are said under its name.
        let joined = self.pages.by_pid.get_mut(&d.pid).and_then(|s| {
            let (name, left) = s.answers_as.as_mut()?;
            let name = name.clone();
            *left = left.saturating_sub(1);
            if *left == 0 {
                s.answers_as = None;
            }
            Some(name)
        });
        let text = match &d.key {
            Some(_) if d.ok && joined.is_some() => format!("{}: {}", joined.unwrap_or_default(), d.text),
            // settingsd's own sentence that begins with the key (CL-5: a stored key binds a provider's address).
            Some(k) if !d.ok && d.text.starts_with(k.as_str()) => d.text.clone(),
            Some(k) => {
                let short = self.page_model.rows.iter().find(|r| r.key == *k).map(|r| r.short.clone()).unwrap_or_else(|| label(self, k));
                format!("{short}: {}", d.text)
            }
            None => d.text.clone(),
        };
        self.page_say(d.pid, text, !d.ok);
    }

    pub fn page_act(&mut self, pid: u32, part: Part) {
        self.pages.real = self.hand_real();
        if let Some(s) = self.pages.by_pid.get_mut(&pid).filter(|s| s.kind == PageKind::Settings) {
            s.jump = false;
        }
        if let Part::Prog(p) = part {
            return self.prog_act(pid, p);
        }
        if let Part::Sys(p) = part {
            return self.sys_act(pid, p);
        }
        if self.page_kind(pid) == PageKind::Ki {
            return self.ki_act(pid, part);
        }
        if self.page_kind(pid) == PageKind::WerHatWas {
            return self.wer_act(pid, part);
        }
        if self.page_kind(pid) == PageKind::Update {
            return self.upd_act(pid, part);
        }
        if self.page_tabs_act(pid, part) {
            return;
        }
        let v = &self.settings;
        // A part of a row that went away with a new registry list does nothing (WY-SET: rows of the tab's view).
        let row = |r: usize| self.page_rows_of(pid).rows.get(r).cloned();
        match part {
            Part::Undo => self.page_send(pid, Req::UNDO),
            Part::DialogCancel => self.page_dialog_close(pid, false),
            Part::DialogOk => self.page_dialog_close(pid, true),
            Part::Choice(r, o) => {
                if let Some(Row { key, kind: Kind::Choice(opts), short, .. }) = row(r) {
                    if let Some((value, _)) = opts.get(o) {
                        // WY-SET: a consolidated row writes the keys under it.
                        let known: std::collections::HashSet<&str> = self.setting_keys.iter().map(|(k, _)| k.as_str()).collect();
                        match crate::page_join::writes(&key, value, &|k| known.contains(k)) {
                            Some(w) => {
                                let name = opts[o].1.clone();
                                self.page_set_many(pid, &short, &name, w)
                            }
                            None => self.page_set(pid, &key, Json::str(value)),
                        }
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
                if let Some(Row { key, kind: Kind::Switch, default, label, .. }) = row(r) {
                    let now = v.flag(&key).unwrap_or(default == "true");
                    // EINS-B: a switch that lets the assistant see more asks first (only Georg's own yes).
                    if crate::page_join::loosens(&key, &now.to_string(), &(!now).to_string()) {
                        self.page_set_many(pid, &label, "an", vec![(key, Json::Bool(!now))]);
                    } else {
                        self.page_set(pid, &key, Json::Bool(!now));
                    }
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
            Part::Link(r) => {
                if let Some(Row { kind: Kind::Link(addr), .. }) = row(r) {
                    eprintln!("bitscomp: page: link opens {addr}");
                    self.open_address(&addr, crate::address::Where::Tab, None);
                }
            }
            // DB4: "Löschen" and "Alles vergessen" of the learned rows (their index among all rows).
            Part::Button(r, _) => {
                let key = row(r).map(|x| x.key).unwrap_or_default();
                if let Some(full) = self.page_model.rows.iter().position(|x| x.key == key) {
                    self.gelernt_button(pid, full);
                }
            }
            // EINS-B: the level's three rows (a change that gives more freedom asks first, `page_set_many`) and the
            // never-see list's programs (a click puts one on or takes it off).
            Part::Pill(r, o) => match row(r) {
                Some(Row { key, kind: Kind::Pills { opts, multi: true, .. }, label, .. }) => {
                    if let Some((value, name)) = opts.get(o) {
                        let list = crate::assist_level::toggled(&self.settings, value);
                        let old = self.settings.text(&key).unwrap_or("").to_string();
                        // A program taken off the list is seen again: asked first, only Georg's own yes.
                        if crate::page_join::loosens(&key, &old, &list) {
                            self.page_set_many(pid, &label, &format!("ohne {name}"), vec![(key, Json::str(&list))]);
                        } else {
                            self.page_set(pid, &key, Json::str(&list));
                        }
                    }
                }
                Some(Row { key, kind: Kind::Pills { opts, .. }, short, .. }) => {
                    if let Some((value, name)) = opts.get(o) {
                        self.page_set_many(pid, &short, name, vec![(key, Json::str(value))]);
                    }
                }
                _ => {}
            },
            Part::Body | Part::Prog(_) | Part::Sys(_) => {}
            // The rail and the tabs' own parts went to `page_tabs_act`.
            Part::Tab(_) | Part::Search | Part::SearchClear | Part::Changed | Part::Filter(_) | Part::Fold(_) | Part::Reset(_) | Part::Sub(..) => {}
        }
    }

    pub(crate) fn page_edit_begin(&mut self, pid: u32, part: Part, text: String) {
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
            Part::Field(r) if self.page_kind(pid) == PageKind::Ki => self.ki_commit(pid, r, e.text),
            Part::Field(r) if self.page_kind(pid) == PageKind::Update => self.upd_commit(pid, r, e.text),
            Part::Field(r) => self.page_commit_field(pid, r, &e.text),
            Part::Sub(r, k) => self.page_commit_sub(pid, r, k, &e.text),
            Part::Search => {}
            Part::Prog(_) => self.prog_commit(pid, &e.text),
            _ => {}
        }
    }

    /// Enter in row `r`'s field: the value goes to settingsd when it fits the row's type (settingsd checks again).
    fn page_commit_field(&mut self, pid: u32, r: usize, text: &str) {
        let Some(row) = self.page_rows_of(pid).rows.get(r).cloned() else { return };
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
        self.pages.real = self.input_origin == crate::input::Origin::Physical;
        match sym {
            Keysym::Escape => {
                // CL-5: his own Esc ends a key's field (an end that is not his makes his next keys go nowhere).
                if self.pages.real {
                    self.ki.typing = None;
                }
                return self.page_edit_end();
            }
            Keysym::Return | Keysym::KP_Enter => return self.page_commit(pid),
            _ => {}
        }
        if self.page_kind(pid) == PageKind::Ki && self.ki_key_guard(pid) {
            return;
        }
        if self.page_kind(pid) == PageKind::Update && self.upd_key_guard(pid) {
            return;
        }
        // GPY-PASTE: the clipboard's text, Georg's own Ctrl+V only.
        if crate::clip_read::is_paste(sym, mods) {
            return self.page_paste(pid);
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
                Some(c) if !c.is_control() && !mods.ctrl && !mods.logo && e.text.chars().count() < crate::page_paste::FIELD_MAX => e.insert(c),
                _ => return,
            },
        }
        self.needs_frame = true;
        self.page_typed(pid);
    }

    /// The text of `pid`'s field changed (a key, a paste): the page follows it.
    pub(crate) fn page_typed(&mut self, pid: u32) {
        match self.page_kind(pid) {
            PageKind::Programme => self.prog_typed(pid),
            PageKind::Ki => self.ki_typed(pid),
            PageKind::System => self.sys_typed(pid),
            PageKind::Settings => self.page_search_typed(pid),
            PageKind::WerHatWas | PageKind::Update => {}
        }
    }

    pub fn page_scroll(&mut self, pid: u32, steps: f64) {
        if let Some(s) = self.pages.by_pid.get_mut(&pid) {
            s.jump = false;
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
        if self.pages.by_pid.keys().all(|p| alive.contains_key(p)) {
            return;
        }
        let gone: Vec<u32> = self.pages.by_pid.keys().filter(|p| !alive.contains_key(p)).copied().collect();
        for pid in &gone {
            // CL-5: what was typed on the page "KI" may have been a key.
            if let Some(PageState { kind: PageKind::Ki, edit: Some((_, e)), .. }) = self.pages.by_pid.remove(pid) {
                crate::settings_write::wipe(e.text);
            }
        }
        self.ki_pages_gone(&gone);
        self.wer_pages_gone();
        self.upd_pages_gone();
        self.sys_pages_gone();
    }

    /// `ctl hot` lines of every visible page's controls: `page pid=N NAME at=X,Y`.
    pub fn page_hot(&self) -> String {
        let s = self.theme.scale;
        let mut out = String::new();
        for p in self.drawn_placements().into_iter().filter(|p| p.visible) {
            let Some(pid) = self.page_pid(&p.win) else { continue };
            let dialog = self.pages.by_pid.get(&pid).and_then(|s| s.confirm.as_ref());
            if self.page_kind(pid) == PageKind::Programme {
                out.push_str(&self.prog_hot(pid, self.content_rect(p.frame), dialog));
                continue;
            }
            if self.page_kind(pid) == PageKind::System {
                out.push_str(&self.sys_hot(pid, self.content_rect(p.frame), dialog));
                continue;
            }
            let Some(lay) = self.page_layout(pid, self.content_rect(p.frame)) else { continue };
            let parts = dialog.map_or_else(|| lay.parts.clone(), |d| crate::page_confirm::parts(lay.content, d));
            let rows = &self.page_rows_of(pid).rows;
            for (part, b) in &parts {
                let (x, y) = (((b.x + b.w / 2) as f64 * s).round(), ((b.y + b.h / 2) as f64 * s).round());
                out.push_str(&format!("page pid={pid} {} at={x},{y}\n", part_name(rows, *part)));
            }
        }
        out
    }

    pub fn page_clear_msgs(&mut self) {
        for s in self.pages.by_pid.values_mut() {
            s.msg = None;
        }
        self.prog.note = None;
        self.page_dialogs_drop(None, "");
        self.needs_frame = true;
    }

    /// `ctl page`: each page's state.
    pub fn page_report(&self) -> String {
        let mut v: Vec<_> = self.pages.by_pid.iter().collect();
        v.sort_by_key(|(p, _)| **p);
        let mut out = String::new();
        for (pid, s) in v {
            if s.kind == PageKind::Programme {
                out.push_str(&self.prog_report(*pid));
                continue;
            }
            let rows = &self.page_rows_of(*pid).rows;
            let edit = s.edit.as_ref().map_or("none".to_string(), |(p, e)| match p {
                // CL-5: a key being typed is not reported, nor its length.
                Part::Field(r) if matches!(rows.get(*r).map(|r| &r.kind), Some(Kind::Secret { .. })) => format!("{}:hidden", part_name(rows, *p)),
                _ => format!("{}:{:?}", part_name(rows, *p), e.text),
            });
            let dialog = s.confirm.as_ref().map_or("none".to_string(), |d| {
                if d.ask.key.is_empty() {
                    return format!("own:{:?}:focus={}", d.title, if d.on_ok { "ok" } else { "cancel" });
                }
                format!("{}:{}:focus={}", if matches!(d.ask.req, Req::Undo { .. }) { "undo" } else { "set" }, d.ask.key, if d.on_ok { "ok" } else { "cancel" })
            });
            let msg = s.msg.as_ref().map_or("none".to_string(), |(m, err)| format!("{}{m:?}", if *err { "error:" } else { "" }));
            out.push_str(&format!(
                "page pid={pid} address={} prefix={} scroll={} edit={edit} dialog={dialog} msg={msg} live={} busy={}{}\n",
                s.address,
                if s.prefix.is_empty() { "-" } else { &s.prefix },
                s.scroll,
                self.settings_live && self.settings_writer.is_some(),
                self.pages.busy,
                match s.kind {
                    PageKind::Ki => self.ki_report(),
                    PageKind::WerHatWas => self.wer_report(),
                    PageKind::Update => self.upd_report(),
                    PageKind::System => self.sys_report(*pid),
                    _ => self.page_tabs_report(*pid),
                }
            ));
        }
        out.push_str("end");
        out
    }
}
