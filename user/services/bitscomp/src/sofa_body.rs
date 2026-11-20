//! GM-6b: the bodies of the sofa mode's Start, Bibliothek and Medien in bitscomp (plan/spec/sofa-mode.md 2.1-2.3,
//! 3.1-3.4): their focus elements (sofa_layout), what A, a click or a double click does on each, the collections
//! (LB/RB, Bild auf/ab, the wheel), the sort button (`sofa.sort` through settingsd), the selection and the grid's
//! scrolling, the launches of 3.2 by the launcher rule onto `Sofa` fullscreen, the play-time record (3.4) and the
//! music player's cached answer (3.5). The model itself is pure (sofa_model.rs).

use std::time::Duration;

use mcpreg::Json;

use crate::pad::Dir;
use crate::playtime::{self, Book};
use crate::sofa_focus::{Item, Screen};
use crate::sofa_layout::{self as lay, StartShape};
use crate::sofa_model::{self as model, Act, Coll, Hero, Input, Tile};
use crate::sofa_music::{self, Job, Music};
use crate::state::App;
use crate::win::Window;

/// A window that maps this soon after a sofa start and that no library entry owns is the started program's
/// (a native Steam game's window carries its own class, not `steam_app_<id>`).
pub const LAUNCH_HINT: Duration = Duration::from_secs(120);
/// Whether an unowned window may be taken by the launch hint: only a top window with a class that is not
/// Steam's own (the client maps first when it was not running; its windows must not stretch the game's run).
pub fn hint_takes(app_id: &str, sub: bool) -> bool {
    let id = app_id.trim().to_ascii_lowercase();
    !sub && !id.is_empty() && !id.starts_with("steam")
}
/// Two clicks on one tile this close are a double click (it starts; one click selects, §4).
pub const DOUBLE: Duration = Duration::from_millis(400);
/// The music player is asked again on a screen change at most this often.
pub const MUSIC_AGAIN: Duration = Duration::from_secs(5);

/// The sofa mode's library state (in `Sofa`).
#[derive(Default)]
pub struct Body {
    /// The collection shown (an index into the screen's column).
    pub coll: usize,
    /// The grid's first row shown.
    pub first: usize,
    /// The selected tile (the selection bar's).
    pub sel: usize,
    pub click: Option<(String, Duration)>,
    /// A failure or refusal to show on the screen (German).
    pub note: Option<String>,
    pub clock: playtime::Clock,
    pub book: Book,
    pub book_loaded: bool,
    /// Runs that ended before the file was read: added once it is there.
    pub book_pending: Vec<(String, i64, i64)>,
    pub book_tx: Option<std::sync::mpsc::Sender<String>>,
    pub music: Option<Music>,
    pub music_tx: Option<std::sync::mpsc::Sender<Job>>,
    pub music_asked: Option<Duration>,
    /// The program a sofa tile started last and until when its first window counts as its own.
    pub launched: Option<(String, Duration)>,
}

impl App {
    /// The play-time file's and the music player's threads (main.rs, once).
    pub fn sofa_body_start(&mut self, handle: &smithay::reexports::calloop::LoopHandle<'static, App>) {
        match playtime::start(handle, playtime::path(), |app: &mut App, b| app.sofa_book_loaded(b)) {
            Ok(tx) => self.sofa.body.book_tx = Some(tx),
            Err(e) => eprintln!("bitscomp: playtime: {e} (play time is not kept)"),
        }
        match sofa_music::start(handle, sofa_music::socket(), |app: &mut App, m| app.sofa_music_came(m)) {
            Ok(tx) => self.sofa.body.music_tx = Some(tx),
            Err(e) => eprintln!("bitscomp: sofa: {e} (no music card)"),
        }
    }

    fn sofa_book_loaded(&mut self, b: Book) {
        let body = &mut self.sofa.body;
        body.book = b;
        body.book_loaded = true;
        let pending = std::mem::take(&mut body.book_pending);
        let any = !pending.is_empty();
        for (id, s, e) in pending {
            self.sofa_book_add(&id, s, e);
        }
        if any {
            self.sofa_book_save();
        }
        self.needs_frame = true;
    }

    fn sofa_music_came(&mut self, m: Option<Music>) {
        if self.sofa.body.music != m {
            self.sofa.body.music = m;
            self.needs_frame = true;
        }
    }

    /// Asks the music player (off the main loop) when a screen that shows it comes, at most every `MUSIC_AGAIN`.
    pub fn sofa_music_ask(&mut self) {
        let now = crate::pacer::now();
        if self.sofa.body.music_asked.is_some_and(|t| now < t + MUSIC_AGAIN) {
            return;
        }
        if let Some(tx) = &self.sofa.body.music_tx
            && tx.send(Job::Ask).is_ok()
        {
            self.sofa.body.music_asked = Some(now);
        }
    }

    pub fn sofa_now(&self) -> i64 {
        if self.status.fixed { crate::status::FIXED_SECS } else { crate::status::epoch_secs() }
    }

    /// Windows exist on any workspace (the YouTube tile waits, decision S1).
    fn sofa_any_windows(&self) -> bool {
        (0..self.ws_count()).any(|i| self.ws_counts(i).1 > 0)
    }

    /// `f` with the model's input of now.
    pub fn sofa_with<R>(&self, f: impl FnOnce(&Input) -> R) -> R {
        let fixed = self.status.fixed;
        let tm = move |s: i64| crate::status::tm_of(s, fixed);
        let i = Input {
            index: &self.library,
            play: &self.sofa.body.book,
            favourites: &self.desk.favourites,
            music: self.sofa.body.music.as_ref(),
            windows: self.sofa_any_windows(),
            sort: &self.sofa.prefs.sort,
            now: self.sofa_now(),
            tm: &tm,
        };
        f(&i)
    }

    /// The collections of the shown screen's column.
    pub fn sofa_colls(&self) -> &'static [Coll] {
        if self.sofa.nav.screen == Screen::Medien { &Coll::MEDIEN } else { &Coll::LIBRARY }
    }

    pub fn sofa_coll(&self) -> Coll {
        let c = self.sofa_colls();
        c[self.sofa.body.coll.min(c.len() - 1)]
    }

    pub fn sofa_tiles(&self) -> Vec<Tile> {
        let c = self.sofa_coll();
        self.sofa_with(|i| model::tiles(c, i))
    }

    pub fn sofa_start_model(&self) -> (Hero, Vec<Tile>) {
        self.sofa_with(|i| {
            let h = model::hero(i);
            let r = model::recent(i, &h, lay::RECENT_MAX);
            (h, r)
        })
    }

    /// The body's focus elements on the shown screen.
    pub fn sofa_body_items(&self) -> Vec<Item> {
        let (bw, bh) = self.sofa_frame().board();
        let m = &crate::sofa_draw::measure;
        match self.sofa.nav.screen {
            Screen::Start => {
                let (h, r) = self.sofa_start_model();
                let mut v = lay::start(bw, StartShape { music: self.sofa.body.music.is_some(), recent: r.len(), game: h.key.is_some() }, m);
                let names: Vec<String> = self.passend_sofa().into_iter().map(|(n, _)| n).collect();
                v.extend(crate::passend::sofa_items(&names, m));
                v
            }
            Screen::Bibliothek | Screen::Medien => lay::library(bw, bh, self.sofa_colls().len(), self.sofa_tiles().len(), self.sofa.body.first, m),
            Screen::Einstellungen => lay::settings(bw, self.sofa_set_rows().len()),
            Screen::Suche => self.sofa_search_items(),
        }
    }

    /// A screen came: the library starts on its first collection (Medien on YouTube), the music player is asked.
    pub fn sofa_body_screen(&mut self, s: Screen) {
        if s != Screen::Suche {
            self.sofa_search_forget_secret();
        }
        let b = &mut self.sofa.body;
        if matches!(s, Screen::Bibliothek | Screen::Medien) {
            b.coll = 0;
            b.first = 0;
            b.sel = 0;
        }
        b.note = None;
        self.sofa.set.note = None;
        if matches!(s, Screen::Start | Screen::Bibliothek | Screen::Medien) {
            self.sofa_music_ask();
        }
    }

    /// The focus moved to `id`: a library tile becomes the selection.
    pub fn sofa_follow(&mut self) {
        if self.sofa.nav.screen == Screen::Suche {
            return self.sofa_search_follow();
        }
        // GM-6c: the focus in Einstellungen' left column shows its section.
        if self.sofa.nav.screen == Screen::Einstellungen
            && let Some(n) = self.sofa.focus.as_deref().and_then(|f| f.strip_prefix("section:")).and_then(|n| n.parse().ok())
        {
            return self.sofa_set_section(n);
        }
        if !matches!(self.sofa.nav.screen, Screen::Bibliothek | Screen::Medien) {
            return;
        }
        if let Some(n) = self.sofa.focus.as_deref().and_then(|f| f.strip_prefix("tile:")).and_then(|n| n.parse().ok()) {
            self.sofa.body.sel = n;
        }
    }

    /// Up and down past the grid's shown rows scroll it (R1: the grid follows the number of games). True when it did.
    pub fn sofa_scroll_move(&mut self, d: Dir) -> bool {
        if !matches!(self.sofa.nav.screen, Screen::Bibliothek | Screen::Medien) || !matches!(d, Dir::Up | Dir::Down) {
            return false;
        }
        let Some(n) = self.sofa.focus.as_deref().and_then(|f| f.strip_prefix("tile:")).and_then(|n| n.parse::<usize>().ok()) else { return false };
        let (bw, bh) = self.sofa_frame().board();
        let rows = lay::lib_geom(bw, bh).rows;
        let count = self.sofa_tiles().len();
        let (row, b) = (n / lay::COLS, &mut self.sofa.body);
        let to = match d {
            Dir::Down if row + 1 >= b.first + rows && (row + 1) * lay::COLS < count => {
                b.first = row + 2 - rows;
                (n + lay::COLS).min(count - 1)
            }
            Dir::Up if row == b.first && b.first > 0 => {
                b.first -= 1;
                n - lay::COLS
            }
            _ => return false,
        };
        b.sel = to;
        self.sofa.focus = Some(format!("tile:{to}"));
        self.needs_frame = true;
        true
    }

    /// LB/RB (Bild auf/ab, the wheel): the previous/next collection in the library, the header pill elsewhere.
    pub fn sofa_lbrb(&mut self, by: i32) -> bool {
        if !matches!(self.sofa.nav.screen, Screen::Bibliothek | Screen::Medien) {
            return false;
        }
        let n = self.sofa_colls().len() as i32;
        let to = (self.sofa.body.coll as i32 + by).clamp(0, n - 1) as usize;
        self.sofa_coll_set(to);
        true
    }

    fn sofa_coll_set(&mut self, to: usize) {
        if to != self.sofa.body.coll {
            let b = &mut self.sofa.body;
            (b.coll, b.first, b.sel) = (to, 0, 0);
            eprintln!("bitscomp: sofa: collection {}", self.sofa_coll().word());
            if self.sofa_coll() == Coll::Musik {
                self.sofa_music_ask();
            }
        }
        // A focused tile that is gone now goes to the first one (or the column's item).
        let items = self.sofa_items();
        if self.sofa.focus.as_deref().is_some_and(|f| f.starts_with("tile:") && !items.iter().any(|i| i.id == f)) {
            self.sofa.focus = Some(if items.iter().any(|i| i.id == "tile:0") { "tile:0".into() } else { format!("coll:{to}") });
        }
        self.needs_frame = true;
    }

    /// `sofa.sort` to its next value (Zuletzt gespielt, A-Z, Größe), kept in settingsd.
    fn sofa_sort_next(&mut self) {
        let next = match self.sofa.prefs.sort.as_str() {
            "zuletzt" => "az",
            "az" => "groesse",
            _ => "zuletzt",
        };
        self.settings.set("sofa.sort", next);
        self.sofa_apply();
        let req = crate::settings_write::Req::set("sofa.sort", Json::str(next));
        let sent = self.settings_live && self.settings_writer.as_ref().is_some_and(|w| w.send(crate::desk::DESK_PID, req));
        eprintln!("bitscomp: sofa: sofa.sort = {next} ({})", if sent { "kept in settingsd" } else { "local only: settingsd not there" });
        self.sofa.body.first = 0;
        self.needs_frame = true;
    }

    /// A click on `id`: in the library grid one click selects and a double click starts; else as A.
    pub fn sofa_click(&mut self, id: &str) {
        let grid = matches!(self.sofa.nav.screen, Screen::Bibliothek | Screen::Medien) && id.starts_with("tile:");
        if !grid {
            return self.sofa_activate(id);
        }
        let now = crate::pacer::now();
        let double = self.sofa.body.click.as_ref().is_some_and(|(c, t)| c == id && now < *t + DOUBLE);
        self.sofa.body.click = if double { None } else { Some((id.to_string(), now)) };
        self.sofa_follow();
        self.needs_frame = true;
        if double {
            self.sofa_activate(id);
        }
    }

    /// A on a body element; false for ids that are not the body's.
    pub fn sofa_body_activate(&mut self, id: &str) -> bool {
        let lib = matches!(self.sofa.nav.screen, Screen::Bibliothek | Screen::Medien);
        let n = id.strip_prefix("tile:").and_then(|n| n.parse::<usize>().ok());
        let c = id.strip_prefix("coll:").and_then(|n| n.parse::<usize>().ok());
        match (id, n, c) {
            ("hero", ..) => {
                let (h, _) = self.sofa_start_model();
                match h.key {
                    Some(k) => self.sofa_run_key(&k),
                    None => self.sofa_go(Screen::Bibliothek),
                }
            }
            ("hero:details", ..) => {
                let (h, _) = self.sofa_start_model();
                self.sofa_go(Screen::Bibliothek);
                if let Some(k) = h.key {
                    self.sofa_select_key(&k);
                }
            }
            ("card:youtube", ..) => {
                let t = self.sofa_with(|i| model::tiles(Coll::YouTube, i)).remove(0);
                self.sofa_run(&t);
            }
            ("card:musik", ..) => {
                self.sofa_go(Screen::Medien);
                self.sofa_coll_set(Coll::MEDIEN.iter().position(|c| *c == Coll::Musik).unwrap_or(0));
            }
            ("alle", ..) => self.sofa_go(Screen::Bibliothek),
            (p, ..) if p.starts_with("passend:") => {
                self.passend_sofa_run(p);
            }
            ("steam", ..) => {
                let t = Tile { act: self.sofa_steam_act(), ..model_tile("steam", "Steam") };
                self.sofa_run(&t);
            }
            ("sort", ..) => self.sofa_sort_next(),
            ("play", ..) => {
                if let Some(t) = self.sofa_tiles().get(self.sofa.body.sel).cloned() {
                    self.sofa_run(&t);
                }
            }
            ("steam-open", ..) => match self.sofa_tiles().get(self.sofa.body.sel).and_then(|t| t.steam) {
                Some(appid) => {
                    let argv = model::steam_argv(&self.library, &format!("steam://nav/games/details/{appid}"));
                    self.sofa_run(&Tile { act: Act::Argv(argv), ..model_tile("steam", "Steam") });
                }
                None => eprintln!("bitscomp: sofa: In Steam öffnen: the selection is no Steam game"),
            },
            (_, Some(n), _) if lib => {
                self.sofa.body.sel = n;
                if let Some(t) = self.sofa_tiles().get(n).cloned() {
                    self.sofa_run(&t);
                }
            }
            (_, Some(n), _) => {
                if let Some(t) = self.sofa_start_model().1.get(n).cloned() {
                    self.sofa_run(&t);
                }
            }
            (_, _, Some(c)) if lib => self.sofa_coll_set(c),
            _ => return false,
        }
        true
    }

    /// The Spiele collection with `key` selected and focused (Details).
    fn sofa_select_key(&mut self, key: &str) {
        self.sofa_coll_set(0);
        let Some(n) = self.sofa_tiles().iter().position(|t| t.key == key) else { return };
        let (bw, bh) = self.sofa_frame().board();
        let rows = lay::lib_geom(bw, bh).rows;
        let b = &mut self.sofa.body;
        b.sel = n;
        b.first = (n / lay::COLS).saturating_sub(rows - 1);
        self.sofa.focus = Some(format!("tile:{n}"));
    }

    fn sofa_run_key(&mut self, key: &str) {
        if let Some(t) = self.sofa_with(|i| model::tiles(Coll::Spiele, i)).into_iter().find(|t| t.key == key) {
            self.sofa_run(&t);
        }
    }

    /// Steam öffnen (3.2): the stick launcher's entry by the launcher rule, else `steam`.
    fn sofa_steam_act(&self) -> Act {
        if self.library.entries.iter().any(|e| e.id == "steam") {
            Act::Start { id: "steam".into(), extra: Vec::new(), restore: true }
        } else {
            Act::Argv(vec!["steam".into()])
        }
    }

    /// What a tile does (3.2): the launcher rule first (the newest minimised window comes back, fullscreen on
    /// `Sofa`), else the program starts and its window lands on `Sofa` (sofa_window_new makes it fullscreen).
    pub fn sofa_run(&mut self, t: &Tile) {
        self.sofa.body.note = None;
        match &t.act {
            Act::Refused(why) => {
                eprintln!("bitscomp: sofa: {} refused: {why}", t.name);
                self.sofa.body.note = Some(why.to_string());
            }
            Act::Music { kind, name } => match &self.sofa.body.music_tx {
                Some(tx) if tx.send(Job::Play { kind, name: name.clone() }).is_ok() => {}
                _ => self.sofa.body.note = Some("Musik: noch nicht da".into()),
            },
            Act::Argv(argv) => self.sofa_spawn(&t.name, None, argv.clone(), None),
            Act::Start { id, extra, restore } => {
                let Some(e) = self.library.entries.iter().find(|e| e.id == *id).cloned() else {
                    eprintln!("bitscomp: sofa: start of {id}: no such program in the library");
                    self.sofa.body.note = Some(format!("„{}“ ist nicht installiert.", t.name));
                    self.needs_frame = true;
                    return;
                };
                if *restore && self.restore_program(&e.address) {
                    eprintln!("bitscomp: sofa: {id} restored (its newest minimised window)");
                    self.recent_add(&e.address);
                    if let Some(w) = self.ws.focused().cloned() {
                        self.sofa_window_new(&w);
                    }
                    return;
                }
                if e.exec.is_empty() {
                    return self.open_address(&e.address, crate::address::Where::Tab, None);
                }
                let mut argv = e.argv();
                argv.extend(extra.iter().cloned());
                self.sofa_spawn(&e.name, Some(&e.address), argv, Some(&e.id));
            }
        }
        self.needs_frame = true;
    }

    fn sofa_spawn(&mut self, name: &str, address: Option<&str>, argv: Vec<String>, id: Option<&str>) {
        match self.open_program(argv.clone(), crate::address::Where::Tile, None) {
            Some(pid) => {
                eprintln!("bitscomp: sofa: started {name} (pid {pid}): {}", argv.join(" "));
                self.lib.starts.push((pid, name.to_string()));
                if let Some(a) = address {
                    self.recent_add(a);
                }
                self.sofa.body.launched = id.map(|i| (i.to_string(), crate::pacer::now() + LAUNCH_HINT));
            }
            None => {
                let prog = argv.first().map_or("?", |s| s.as_str());
                eprintln!("bitscomp: sofa: start of {name} failed: {prog} not found or not executable");
                self.sofa.body.note = Some(format!("„{name}“ ließ sich nicht starten: {prog} fehlt oder ist nicht ausführbar."));
            }
        }
    }

    /// A window mapped (any workspace): its program's play time runs from now (3.4).
    pub fn play_mapped(&mut self, w: &Window) {
        let win = crate::ctl::win_id(w);
        if self.sofa.body.clock.counts(win) {
            return;
        }
        let app_id = crate::ctl::names(w).0;
        let ix = &self.library;
        let owner = ix.entries.iter().filter(|e| matches!(e.source, crate::library::Source::Steam(_))).chain(ix.entries.iter()).find(|e| e.owns(&app_id)).map(|e| e.id.clone());
        let sub = w.toplevel().is_some_and(|t| t.parent().is_some()) || w.x11().is_some_and(|x| x.override_redirect || x.transient_for.get() != 0);
        let hint = self.sofa.body.launched.as_ref().filter(|(_, t)| crate::pacer::now() < *t).map(|(i, _)| i.clone())
            .filter(|i| ix.entries.iter().any(|e| e.id == *i && matches!(e.source, crate::library::Source::Steam(_))))
            .filter(|_| owner.is_none() && hint_takes(&app_id, sub));
        let Some(prog) = owner.or(hint) else { return };
        if self.sofa.body.launched.as_ref().is_some_and(|(i, _)| *i == prog) {
            self.sofa.body.launched = None;
        }
        let now = self.sofa_now();
        self.sofa.body.clock.mapped(win, &prog, now);
        eprintln!("bitscomp: playtime: {prog} runs (window {win})");
    }

    /// A window went: its program's run ends with its last window and is kept.
    pub fn play_gone(&mut self, w: &Window) {
        let now = self.sofa_now();
        let Some((prog, start, end)) = self.sofa.body.clock.gone(crate::ctl::win_id(w), now) else { return };
        eprintln!("bitscomp: playtime: {prog} ended after {} s", (end - start).max(0));
        if !self.sofa.body.book_loaded {
            self.sofa.body.book_pending.push((prog, start, end));
            return;
        }
        self.sofa_book_add(&prog, start, end);
        self.sofa_book_save();
    }

    fn sofa_book_add(&mut self, id: &str, start: i64, end: i64) {
        let fixed = self.status.fixed;
        self.sofa.body.book.add(id, start, end, |t| {
            let tm = crate::status::tm_of(t, fixed);
            (tm.tm_year as i64) * 400 + tm.tm_yday as i64
        });
        self.needs_frame = true;
    }

    fn sofa_book_save(&mut self) {
        let text = self.sofa.body.book.encode();
        if let Some(tx) = &self.sofa.body.book_tx
            && tx.send(text).is_err()
        {
            eprintln!("bitscomp: playtime: the file's thread ended: not written");
        }
    }

    /// What the shown screen's body draws (None: no body of GM-6b's on it).
    pub fn sofa_body_look(&self) -> Option<crate::sofa_draw::BodyLook> {
        use crate::sofa_draw::BodyLook;
        let (bw, bh) = self.sofa_frame().board();
        let frame = self.sofa_frame();
        let items = self.sofa_body_items();
        let (focus, note, motion) = (self.sofa.focus.clone(), self.sofa.body.note.clone(), self.sofa.prefs.motion);
        match self.sofa.nav.screen {
            Screen::Start => {
                let (hero, recent) = self.sofa_start_model();
                let youtube = self.sofa_with(|i| model::tiles(Coll::YouTube, i)).remove(0);
                let music = self.sofa.body.music.clone();
                let passend = self.passend_sofa();
                Some(BodyLook::Start(crate::sofa_start_draw::StartLook { frame, bw, bh, hero, recent, youtube, music, items, focus, note, motion, passend }))
            }
            Screen::Bibliothek | Screen::Medien => {
                let coll = self.sofa_coll();
                let (colls, tiles, info) = self.sofa_with(|i| {
                    let colls = self.sofa_colls().iter().map(|c| (c.title(), model::count(*c, i))).collect();
                    let t = model::tiles(coll, i);
                    let info = model::info(coll, &t, i);
                    (colls, t, info)
                });
                let sel = tiles.get(self.sofa.body.sel).cloned();
                let facts = sel.as_ref().map(model::facts).unwrap_or_default();
                Some(BodyLook::Library(crate::sofa_library_draw::LibraryLook {
                    frame,
                    bw,
                    bh,
                    colls,
                    active: self.sofa.body.coll,
                    sort: self.sofa.prefs.sort.clone(),
                    title: coll.title().into(),
                    info,
                    tiles,
                    first: self.sofa.body.first,
                    sel,
                    facts,
                    items,
                    focus,
                    note,
                    motion,
                }))
            }
            Screen::Einstellungen => Some(BodyLook::Settings(self.sofa_set_look())),
            Screen::Suche => Some(BodyLook::Search(self.sofa_search_look())),
        }
    }

    /// `ctl sofa state`'s words of the body (appended after GM-6a's).
    pub fn sofa_body_state(&self) -> String {
        let lib = matches!(self.sofa.nav.screen, Screen::Bibliothek | Screen::Medien);
        let tiles = if lib { self.sofa_tiles() } else { self.sofa_start_model().1 };
        let sel = if lib { tiles.get(self.sofa.body.sel).map_or("-".to_string(), |t| t.key.clone()) } else { "-".into() };
        let (h, _) = self.sofa_start_model();
        format!(
            " coll={} sort={} sel={sel} first={} tiles={} hero={} music={}",
            self.sofa_coll().word(),
            self.sofa.prefs.sort,
            self.sofa.body.first,
            tiles.len(),
            h.key.as_deref().unwrap_or("-"),
            if self.sofa.body.music.is_some() { "yes" } else { "no" },
        )
    }
}

pub(crate) fn model_tile(key: &str, name: &str) -> Tile {
    Tile { key: key.into(), name: name.into(), meta: String::new(), colour: None, greyed: false, act: Act::Refused(""), last: 0, size: 0, steam: None, proton: false, loading: false }
}
