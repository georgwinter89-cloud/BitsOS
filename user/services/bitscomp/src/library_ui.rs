//! PK-L1b: the program library's overlay (desk-PK67-plan.md §3, Georg 6.11.: "Programme per Knopfdruck
//! starten"): opened by the Win key alone (D7, `win_alone`), the top bar's "Programme" button or `ctl library open`;
//! closed by Esc, Win again, a click outside it or a start. Typing filters at once, the arrows move the focus, Tab
//! moves between the rows, Enter or a click starts. A start follows desktop.md's launcher rule: the program's newest
//! minimised window on the shown workspace comes back, else it starts (launch.rs's mark until its window maps). A
//! right click opens the circle menu (pin to the start page, a new tile, two greyed entries). What it shows and the
//! focus moves are pure (`library_view`); its look is `library_draw`.

use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::address::Where;
use crate::layout::Rect;
use crate::library_view::{self as view, Move, Row};
use crate::state::App;

#[derive(Default)]
pub struct Overlay {
    pub open: bool,
    pub query: String,
    /// The focused tile: its row and entry id (kept by id: the index is swapped whole on a rescan).
    pub focus: Option<(usize, String)>,
    pub hover: Option<(usize, String)>,
    /// Logical px the tiles are scrolled up by.
    pub scroll: i32,
    /// A line for the user: a start that failed, a pin.
    pub note: Option<String>,
    /// The entry the circle menu's picked item acts on (set only while that item runs: menu_input.rs).
    pub menu_for: Option<String>,
    pub alone: crate::win_alone::WinAlone,
    /// PK-L1c: the pad's Y put the focus on the search field (Down or B leaves it; typing works either way).
    pub search_focus: bool,
    /// The rows of the last `library_rows` and what they were made from: pointer motion (up to 1000 Hz) and a
    /// frame read them without scoring the whole index again.
    rows_cache: std::cell::RefCell<Option<(RowsKey, Vec<Row>)>>,
    /// Programs the library started that have no window yet: one that ends with an error says so.
    pub starts: Vec<(u32, String)>,
    pub images: crate::library_draw::Images,
}

/// One drawn tile: row, index in it, entry id, its place on the screen (logical px).
pub struct Placed {
    pub row: usize,
    pub i: usize,
    pub id: String,
    pub rect: Rect,
}

/// What the rows depend on: the index (swapped whole on a rescan), the favourites, the recent list, the search.
#[derive(PartialEq)]
struct RowsKey {
    index: usize,
    favourites: Vec<String>,
    recent: Vec<String>,
    query: String,
}

fn is_modifier(sym: Keysym) -> bool {
    matches!(
        sym,
        Keysym::Shift_L
            | Keysym::Shift_R
            | Keysym::Control_L
            | Keysym::Control_R
            | Keysym::Alt_L
            | Keysym::Alt_R
            | Keysym::Super_L
            | Keysym::Super_R
            | Keysym::Meta_L
            | Keysym::Meta_R
            | Keysym::Hyper_L
            | Keysym::Hyper_R
            | Keysym::ISO_Level3_Shift
            | Keysym::ISO_Level5_Shift
            | Keysym::Caps_Lock
            | Keysym::Num_Lock
    )
}

impl App {
    pub fn library_rows(&self) -> Vec<Row> {
        let key = RowsKey {
            index: std::sync::Arc::as_ptr(&self.library) as usize,
            favourites: self.desk.favourites.clone(),
            recent: self.session.recent_addresses(),
            query: self.lib.query.clone(),
        };
        let mut cache = self.lib.rows_cache.borrow_mut();
        match cache.as_ref() {
            Some((k, rows)) if *k == key => rows.clone(),
            _ => {
                let rows = view::rows(&self.library, &key.favourites, &key.recent, &key.query);
                *cache = Some((key, rows.clone()));
                rows
            }
        }
    }

    pub fn library_geo(&self) -> view::Geo {
        view::geo(self.area_full(), self.chrome().top.h, &self.theme)
    }

    /// The focus as (row, index) in `rows`: its id in its row, else in any row, else the first tile.
    fn library_at(&self, rows: &[Row]) -> Option<(usize, usize)> {
        view::locate(rows, self.lib.focus.as_ref())
    }

    /// Every tile in content coordinates (y from the body's top, before scrolling), with each row's heading.
    fn library_layout(&self, rows: &[Row]) -> (view::Geo, Vec<(Rect, Vec<Rect>)>, i32) {
        let g = self.library_geo();
        let lens: Vec<usize> = rows.iter().map(|r| r.ids.len()).collect();
        let (placed, content) = view::place(&g, &lens);
        (g, placed, content)
    }

    /// The tiles whose whole box lies inside the body (what is drawn and can be clicked), on the screen.
    pub fn library_tiles(&self) -> (Vec<Row>, Vec<(Rect, &'static str)>, Vec<Placed>) {
        let rows = self.library_rows();
        let (g, placed, _) = self.library_layout(&rows);
        let shift = g.body.y - self.lib.scroll;
        let inside = |r: Rect| r.y >= g.body.y && r.y + r.h <= g.body.y + g.body.h;
        let mut heads = Vec::new();
        let mut tiles = Vec::new();
        for (k, (head, rects)) in placed.iter().enumerate() {
            let h = Rect { y: head.y + shift, ..*head };
            if inside(h) {
                heads.push((h, rows[k].title));
            }
            for (i, r) in rects.iter().enumerate() {
                let s = Rect { y: r.y + shift, ..*r };
                if inside(s) {
                    tiles.push(Placed { row: k, i, id: rows[k].ids[i].clone(), rect: s });
                }
            }
        }
        (rows, heads, tiles)
    }

    pub fn library_toggle(&mut self, why: &str) {
        if self.lib.open { self.library_close(why) } else { self.library_open(why) }
    }

    pub fn library_open(&mut self, why: &str) {
        if self.lib.open {
            return;
        }
        self.address_end();
        self.menu = None;
        let lib = &mut self.lib;
        (lib.open, lib.query, lib.scroll, lib.note, lib.hover, lib.menu_for, lib.search_focus) = (true, String::new(), 0, None, None, None, false);
        let rows = self.library_rows();
        self.lib.focus = rows.first().and_then(|r| r.ids.first()).map(|id| (0, id.clone()));
        eprintln!("bitscomp: library: opened ({why}), {} programs", self.library.entries.len());
        self.hover = None;
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    pub fn library_close(&mut self, why: &str) {
        if !self.lib.open {
            return;
        }
        self.lib.open = false;
        self.lib.search_focus = false;
        self.lib.query.clear();
        self.lib.note = None;
        if self.menu.as_ref().is_some_and(|m| m.entry.is_some()) {
            self.menu = None;
        }
        eprintln!("bitscomp: library: closed ({why})");
        self.needs_frame = true;
        self.motion(self.now_ms());
    }

    /// A key while the overlay is open; false for the modifiers: they reach the client as before, so neither the
    /// opening Win key nor a held Shift is ever left pressed in it.
    pub fn library_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        if is_modifier(sym) {
            return false;
        }
        match sym {
            Keysym::Escape => self.library_close("Esc"),
            Keysym::Return | Keysym::KP_Enter => self.library_start_focused(),
            Keysym::Left | Keysym::KP_Left => self.library_move(Move::Left),
            Keysym::Right | Keysym::KP_Right => self.library_move(Move::Right),
            Keysym::Up | Keysym::KP_Up => self.library_move(Move::Up),
            Keysym::Down | Keysym::KP_Down => self.library_move(Move::Down),
            Keysym::ISO_Left_Tab => self.library_move(Move::PrevRow),
            Keysym::Tab => self.library_move(if mods.shift { Move::PrevRow } else { Move::NextRow }),
            Keysym::BackSpace => {
                if self.lib.query.pop().is_some() {
                    self.library_searched();
                }
            }
            _ => match sym.key_char() {
                Some(c) if !c.is_control() && !mods.ctrl && !mods.alt && !mods.logo => {
                    self.lib.query.push(c);
                    self.library_searched();
                }
                _ => {}
            },
        }
        true
    }

    /// The search changed: the best match is focused, the list starts at its top.
    fn library_searched(&mut self) {
        let rows = self.library_rows();
        self.lib.focus = rows.first().and_then(|r| r.ids.first()).map(|id| (0, id.clone()));
        self.lib.scroll = 0;
        self.lib.note = rows.is_empty().then(|| format!("Kein Programm passt zu „{}“.", self.lib.query));
        self.needs_frame = true;
    }

    /// Enter or the pad's A: the focused tile starts.
    pub fn library_start_focused(&mut self) {
        let rows = self.library_rows();
        if let Some((r, i)) = self.library_at(&rows) {
            self.library_start(&rows[r].ids[i].clone(), Where::Tab);
        }
    }

    pub fn library_move(&mut self, m: Move) {
        self.lib.search_focus = false;
        let rows = self.library_rows();
        let Some(at) = self.library_at(&rows) else { return };
        let (g, placed, content) = self.library_layout(&rows);
        let lens: Vec<usize> = rows.iter().map(|r| r.ids.len()).collect();
        let (r, i) = view::step(&lens, g.cols, at, m);
        self.lib.focus = Some((r, rows[r].ids[i].clone()));
        let first_line = i < g.cols;
        self.lib.scroll = view::scroll_to(self.lib.scroll, placed[r].1[i], first_line.then_some(placed[r].0), g.body.h, content);
        self.needs_frame = true;
    }

    /// Starts entry `id` by the launcher rule; `w` Tile ("In neuer Kachel öffnen") always starts a new one.
    pub fn library_start(&mut self, id: &str, w: Where) {
        let Some(e) = self.library.entries.iter().find(|e| e.id == id).cloned() else {
            eprintln!("bitscomp: library: start of {id}: no such program any more");
            self.lib.note = Some("Dieses Programm gibt es nicht mehr.".into());
            self.needs_frame = true;
            return;
        };
        if w == Where::Tab && self.restore_program(&e.address) {
            eprintln!("bitscomp: library: {id} restored (its newest minimised window)");
            self.recent_add(&e.address);
            return self.library_close("restored");
        }
        if e.exec.is_empty() {
            // Einstellungen: bitscomp's own page, opened by its address.
            self.library_close("started");
            return self.open_address(&e.address, w, None);
        }
        let argv = e.argv();
        match self.open_program(argv.clone(), w, None) {
            Some(pid) => {
                eprintln!("bitscomp: library: started {id} (pid {pid}): {}", argv.join(" "));
                self.lib.starts.push((pid, e.name.clone()));
                self.recent_add(&e.address);
                self.library_close("started");
            }
            None => {
                let prog = argv.first().map_or("?", |s| s.as_str());
                eprintln!("bitscomp: library: start of {id} failed: {prog} not found or not executable");
                self.lib.note = Some(format!("„{}“ ließ sich nicht starten: {prog} fehlt oder ist nicht ausführbar.", e.name));
                self.needs_frame = true;
            }
        }
    }

    /// A child ended (`reap_children`, before the launch marks are updated). A program the library started that
    /// ends with an error before it showed a window (exec failed in the trampoline, a launcher that gave up) says so:
    /// in the overlay when it is open, else as a notice.
    pub fn library_ended(&mut self, pid: u32, status: std::process::ExitStatus) {
        let Some(k) = self.lib.starts.iter().position(|(p, _)| *p == pid) else { return };
        let (_, name) = self.lib.starts.remove(k);
        let windowless = self.launches.waiting_for(pid);
        self.lib.starts.retain(|(p, _)| self.launches.waiting_for(*p));
        if !windowless || status.success() {
            return;
        }
        eprintln!("bitscomp: library: {name} (pid {pid}) ended before its window: {status}");
        let line = format!("„{name}“ hat sich gleich wieder beendet ({}).", german_status(status));
        if self.lib.open {
            self.lib.note = Some(line);
            self.needs_frame = true;
        } else {
            self.notice(line);
        }
    }

    fn library_tile_at(&self, x: f64, y: f64) -> Option<(usize, String)> {
        self.library_tiles().2.into_iter().find(|p| p.rect.contains(x, y)).map(|p| (p.row, p.id))
    }

    /// A press while the overlay is open (the circle menu, when open, gets it first).
    pub fn library_press(&mut self, code: u32) {
        use crate::pointer::{BTN_LEFT, BTN_RIGHT};
        let (x, y) = (self.pointer.x, self.pointer.y);
        if self.chrome().hit(x, y) == Some(crate::chrome::Part::Programs) {
            return self.library_close("Programme button");
        }
        if !self.library_geo().panel.contains(x, y) {
            return self.library_close("click outside");
        }
        let Some((row, id)) = self.library_tile_at(x, y) else { return };
        self.lib.focus = Some((row, id.clone()));
        match code {
            BTN_LEFT => self.library_start(&id, Where::Tab),
            BTN_RIGHT => self.library_menu(id),
            _ => {}
        }
        self.needs_frame = true;
    }

    /// The circle menu of a tile (the pad's X opens it too, PK-L1c).
    pub fn library_menu(&mut self, id: String) {
        let pinned = view::pinned(&self.library, &self.desk.favourites, &id);
        self.open_menu_with(None, crate::menu::library_items(pinned), Some(id));
    }

    /// PK-L1c: the pad's X: the circle menu centred on the focused tile (the pointer stays where it is; the pad
    /// picks an item by direction, `pad_app`).
    pub fn library_menu_on_focus(&mut self) {
        let (rows, _, tiles) = self.library_tiles();
        let Some((r, i)) = self.library_at(&rows) else { return };
        let id = rows[r].ids[i].clone();
        let g = self.library_geo();
        let rect = tiles.iter().find(|p| p.row == r && p.id == id).map_or(g.panel, |p| p.rect);
        let at = (rect.x as f64 + rect.w as f64 / 2.0, rect.y as f64 + rect.h as f64 / 2.0);
        let pinned = view::pinned(&self.library, &self.desk.favourites, &id);
        eprintln!("bitscomp: library: circle menu of {id} (pad X)");
        self.open_menu_at(None, crate::menu::library_items(pinned), Some(id), at, false);
    }

    pub fn library_motion(&mut self) {
        let hover = self.library_tile_at(self.pointer.x, self.pointer.y);
        if hover != self.lib.hover {
            self.lib.hover = hover;
            self.needs_frame = true;
        }
    }

    pub fn library_wheel(&mut self, steps: f64) {
        let rows = self.library_rows();
        let (g, _, content) = self.library_layout(&rows);
        let s = view::wheel(self.lib.scroll, steps, g.cell.1 / 3, g.body.h, content);
        if s != self.lib.scroll {
            self.lib.scroll = s;
            self.library_motion();
            self.needs_frame = true;
        }
    }

    /// "An Startseite anheften" / "Lösen" of the menu's tile: `desktop.favourites` (the start page and the
    /// favourites bar read it) set at once and kept in settingsd, whose history undoes it as every change.
    pub fn library_pin(&mut self) {
        let Some(id) = self.lib.menu_for.take() else { return };
        let Some(name) = self.library.entries.iter().find(|e| e.id == id).map(|e| e.name.clone()) else { return };
        let was = view::pinned(&self.library, &self.desk.favourites, &id);
        let list = if was { view::unpin(&self.library, &self.desk.favourites, &id) } else { view::pin(&self.library, &self.desk.favourites, &id) };
        let value = list.join(", ");
        let key = crate::page::FAVOURITES;
        self.settings.set(key, &value);
        self.apply_settings(key);
        let req = crate::settings_write::Req::set(key, mcpreg::Json::str(&value));
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(crate::desk::DESK_PID, req));
        eprintln!(
            "bitscomp: library: {id} {} the favourites ({}): {value}",
            if was { "taken from" } else { "added to" },
            if sent { "kept in settingsd" } else { "local only: settingsd not there" }
        );
        let done = if was { format!("„{name}“ ist von der Startseite gelöst.") } else { format!("„{name}“ ist an der Startseite angeheftet.") };
        self.lib.note = Some(if sent { done } else { format!("{done} Nur bis zum Neustart: der Einstellungsdienst antwortet nicht.") });
        self.chrome_dirty();
        self.needs_frame = true;
    }

    /// "In neuer Kachel öffnen" of the menu's tile.
    pub fn library_new_tile(&mut self) {
        if let Some(id) = self.lib.menu_for.take() {
            self.library_start(&id, Where::Tile);
        }
    }

    /// `ctl library state`: whether it is open, the search, the focus and every tile with its screen point.
    pub fn library_state(&self) -> String {
        let s = self.theme.scale;
        let pt = |r: Rect| format!("{},{}", ((r.x as f64 + r.w as f64 / 2.0) * s).round(), ((r.y as f64 + r.h as f64 / 2.0) * s).round());
        let (rows, _, tiles) = self.library_tiles();
        let focus = self.library_at(&rows).map_or("-".to_string(), |(r, i)| rows[r].ids[i].clone());
        let mut out = format!(
            "library open={} query={:?} focus={focus} search_focus={} scroll={} cols={} note={:?}\nfavourites={}\n",
            self.lib.open,
            self.lib.query,
            self.lib.search_focus,
            self.lib.scroll,
            self.library_geo().cols,
            self.lib.note.as_deref().unwrap_or(""),
            self.desk.favourites.join(",")
        );
        for (k, r) in rows.iter().enumerate() {
            out.push_str(&format!("row {k} title={:?} ids={}\n", r.title, r.ids.join(",")));
        }
        for p in tiles.iter().filter(|_| self.lib.open) {
            let pinned = view::pinned(&self.library, &self.desk.favourites, &p.id);
            out.push_str(&format!("tile row={} i={} id={} at={} pinned={pinned}\n", p.row, p.i, p.id, pt(p.rect)));
        }
        out.push_str("end");
        out
    }
}

/// An exit status in the user's words.
fn german_status(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(127), _) => "Programmdatei fehlt oder ist nicht ausführbar".into(),
        (Some(c), _) => format!("Fehlercode {c}"),
        (None, Some(s)) => format!("Signal {s}"),
        _ => "unbekannter Grund".into(),
    }
}
