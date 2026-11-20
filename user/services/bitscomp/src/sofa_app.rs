//! GM-6a: the sofa mode in bitscomp (plan/spec/sofa-mode.md §0, §4, §5, §6): switching in and out (onto the
//! `Sofa` workspace and back to the one shown before; no program is ever closed, paused or restarted), the pad,
//! keys and pointer on the sofa screens (one focus, `sofa_focus`), the routing behind a program in front (the pad
//! is the program's except Home), its windows made fullscreen (WY-8), the settings keys registered at run time,
//! and `ctl sofa state|on|off|focus`. The rules themselves are pure (sofa.rs, sofa_focus.rs, sofa_layout.rs).

use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::pad::{Btn, Dir, Press};
use crate::sofa::{self, Ctx, Frame, Front, Prefs, Show, Step, Way};
use crate::sofa_focus::{self as focus, Area, Back, Item, Screen};
use crate::sofa_layout as lay;
use crate::sofa_search::Target;
use crate::state::App;
use crate::win::Window;

/// `ctl sofa ...`; None for other commands.
pub fn ctl(app: &mut App, words: &[&str]) -> Option<Result<String, String>> {
    if words.first() != Some(&"sofa") {
        return None;
    }
    Some(match words.get(1).copied() {
        None | Some("state") => Ok(app.sofa_state()),
        Some("on") => {
            app.sofa_switch(Way::CtlOn);
            Ok(app.sofa_state())
        }
        Some("off") => {
            app.sofa_switch(Way::CtlOff);
            Ok(app.sofa_state())
        }
        Some("focus") => match words.get(2).copied() {
            None => Ok(app.sofa.focus.clone().unwrap_or_else(|| "-".into())),
            Some(w) => app.sofa_focus_ctl(w).map(|_| app.sofa_state()),
        },
        Some(o) => Err(format!("sofa {o}? (sofa state|on|off|focus [up|down|left|right|a|b|x|y|start|lb|rb|ID])")),
    })
}

/// The volume word while no value is known (GM-6c reads it from the ALSA control).
pub const VOLUME_UNKNOWN: &str = "Ton –";

/// The network word of the header from the cached status (B4: nothing is read here).
fn net_word(blocks: &[crate::status::Block], ssid: Option<&str>) -> String {
    match blocks.iter().find(|b| b.label == "Netz").map(|b| b.value.as_str()) {
        Some("Kabel") => "Kabel".into(),
        Some("WLAN") => match ssid.map(str::trim).filter(|s| !s.is_empty()) {
            Some(n) => format!("WLAN {n}"),
            None => "WLAN".into(),
        },
        _ => "Kein Netz".into(),
    }
}

impl App {
    /// The sofa layout's frame on this output (k and the overscan inset).
    pub fn sofa_frame(&self) -> Frame {
        let (w, h) = crate::chrome_render::wallpaper::output_px(self);
        Frame::of(w, h, self.sofa.prefs.overscan)
    }

    /// The header's status words (network, volume) and the clock ("20:14"). The volume's slot shows "Ton –" until
    /// GM-6c's ALSA control client gives the value (§7), so the header keeps its places.
    pub fn sofa_status(&self) -> (Vec<String>, String) {
        let words = vec![net_word(&self.status.blocks, self.settings.text("net.wifi.ssid")), crate::sofa_quick::header_word(self.sofa.quick.io.volume)];
        let clock = crate::chrome_draw::strip::hour_minute(&self.status.clock).map_or_else(String::new, |(h, m)| format!("{h:02}:{m:02}"));
        (words, clock)
    }

    pub fn sofa_header(&self) -> Option<lay::Header> {
        let s = self.sofa.nav.screen;
        if s == Screen::Suche {
            return None;
        }
        let (bw, _) = self.sofa_frame().board();
        let (words, clock) = self.sofa_status();
        Some(lay::header(bw, s, &words, &clock, &crate::sofa_draw::measure))
    }

    /// The focusable elements shown now (board px): a question's Ja and Nein alone; else the quick menu's (GM-6c);
    /// else the header's and the body's (Start, Bibliothek, Medien: GM-6b; Einstellungen: GM-6c; Suche: GM-6d).
    pub fn sofa_items(&self) -> Vec<Item> {
        if self.sofa.show() == Show::None {
            return Vec::new();
        }
        if self.sofa.confirm.is_some() {
            let (bw, bh) = self.sofa_frame().board();
            return lay::confirm(bw, bh, self.sofa.nav.quick);
        }
        if self.sofa.nav.quick {
            return self.sofa_quick_items();
        }
        if self.sofa.show() != Show::Shell {
            return Vec::new();
        }
        let mut v = self.sofa_header().map(|h| h.items).unwrap_or_default();
        v.extend(self.sofa_body_items());
        v
    }

    /// The footer's hints (buttons for the mouse) of the shown screen.
    pub fn sofa_hints(&self) -> Vec<lay::Placed> {
        if self.sofa.show() != Show::Shell || self.sofa.nav.quick {
            return Vec::new();
        }
        let (bw, bh) = self.sofa_frame().board();
        lay::footer(bw, bh, self.sofa.nav.screen, &crate::sofa_draw::measure)
    }

    /// The element a screen's focus starts on: the board's (hero tile, first library tile, first section, first
    /// key), else the screen's pill, else the first one.
    fn sofa_focus_default(&mut self) {
        let items = self.sofa_items();
        let s = self.sofa.nav.screen;
        let first = match s {
            Screen::Start => "hero",
            Screen::Bibliothek | Screen::Medien => "tile:0",
            Screen::Einstellungen => "",
            Screen::Suche => "key:1",
        };
        let first = if s == Screen::Einstellungen { format!("section:{}", self.sofa.set.section) } else { first.to_string() };
        let pick = [Some(first), s.pill()].into_iter().flatten().find(|id| focus::index(&items, id).is_some());
        self.sofa.focus = pick.or_else(|| items.first().map(|i| i.id.clone()));
        self.needs_frame = true;
    }

    pub fn sofa_state(&self) -> String {
        let s = &self.sofa;
        let f = self.sofa_frame();
        let sofa_ws = self.desk.names.iter().position(|n| n == sofa::WORKSPACE);
        let on = |b: bool| if b { "on" } else { "off" };
        let front = match s.front {
            Front::Shell => "shell",
            Front::Program => "program",
        };
        let keys = if self.sofa_keys_known() { "registered" } else if s.keys_sent { "sent" } else { "local" };
        format!(
            "sofa active={} front={front} quick={} screen={} focus={} shown={} ws={} sofa_ws={} before={:?} windows={} k={:.3} frame={},{} {}x{} start_on_pad={} chord={} overscan={} keys={keys}",
            on(s.active),
            on(s.nav.quick),
            s.nav.screen.word(),
            s.focus.as_deref().unwrap_or("-"),
            match s.show() {
                Show::None => "none",
                Show::Shell => "shell",
                Show::Over => "over",
            },
            self.desk.names.get(self.desk.active).map_or("-", String::as_str).replace(' ', "_"),
            sofa_ws.map_or_else(|| "-".to_string(), |i| (i + 1).to_string()),
            s.before.as_deref().unwrap_or(""),
            sofa_ws.map_or(0, |i| self.ws_counts(i).1),
            f.k,
            f.x,
            f.y,
            f.w,
            f.h,
            s.prefs.start_on_pad,
            if s.prefs.chord { "select_start" } else { "aus" },
            s.prefs.overscan,
        ) + &self.sofa_body_state()
            + &self.sofa_quick_state()
            + &self.sofa_search_state()
    }

    /// A way in or out (§5).
    pub fn sofa_switch(&mut self, way: Way) {
        let ctx = Ctx { active: self.sofa.active, program: self.sofa.front == Front::Program, quick: self.sofa.nav.quick, start_on_pad: self.sofa.prefs.start_on_pad };
        match sofa::decide(ctx, way) {
            Step::Enter => self.sofa_enter(way),
            Step::Leave => self.sofa_leave(way),
            Step::QuickOpen => {
                self.sofa.nav.quick = true;
                eprintln!("bitscomp: sofa: quick menu opened over the program in front ({})", way.word());
                self.sofa_quick_opened();
                self.sofa_changed();
            }
            Step::QuickClose => self.sofa_quick_close(way.word()),
            Step::Stay => {
                if matches!(way, Way::PadPlug | Way::Boot) && !self.sofa.active {
                    eprintln!("bitscomp: sofa: {}: sofa.start_on_pad is off, the desktop stays", way.word());
                }
            }
        }
    }

    fn sofa_quick_close(&mut self, why: &str) {
        self.sofa.nav.quick = false;
        self.sofa.confirm = None;
        eprintln!("bitscomp: sofa: quick menu closed ({why})");
        self.sofa_quick_left();
        self.sofa_changed();
    }

    /// After the quick menu: a focus of the panel goes to the screen's own default.
    fn sofa_quick_left(&mut self) {
        let items = self.sofa_items();
        if !self.sofa.focus.as_deref().is_some_and(|f| focus::index(&items, f).is_some()) {
            self.sofa_focus_default();
        }
    }

    pub(crate) fn sofa_changed(&mut self) {
        self.needs_frame = true;
        self.layout_dirty = true;
        self.chrome_dirty();
        let t = self.now_ms();
        self.motion(t);
    }

    /// The `Sofa` workspace's index (made on the first start, after the others).
    fn sofa_ws(&mut self) -> usize {
        if let Some(i) = self.desk.names.iter().position(|n| n == sofa::WORKSPACE) {
            return i;
        }
        self.desk.names.push(sofa::WORKSPACE.into());
        self.desk.parked.push(Default::default());
        eprintln!("bitscomp: sofa: workspace {} made (kept in the workspace list)", sofa::WORKSPACE);
        self.absicht_register();
        self.session_dirty();
        self.desk.names.len() - 1
    }

    fn sofa_enter(&mut self, way: Way) {
        self.habit_event("sofa", vec![("on", mcpreg::Json::Bool(true))]);
        self.library_close("sofa mode");
        // Every way in (ctl, a pad plugged in, the start on the pad), not only Win+Esc: the view and its question
        // never stay reachable under the sofa mode's screens.
        self.rewind_close("sofa mode");
        self.address_end();
        self.menu = None;
        let shown = self.desk.names.get(self.desk.active).cloned();
        let i = self.sofa_ws();
        if shown.as_deref() != Some(sofa::WORKSPACE) {
            self.sofa.before = shown;
        }
        self.ws_switch(i);
        let s = &mut self.sofa;
        s.active = true;
        s.nav = Default::default();
        s.hover = None;
        s.search = Default::default();
        s.confirm = None;
        s.set.learn = None;
        s.front = if self.ws.tiles.is_empty() && self.ws.floating.is_empty() { Front::Shell } else { Front::Program };
        if s.front == Front::Program
            && let Some(w) = self.ws.focused().cloned()
            && !self.ws.is_fullscreen(&w)
        {
            self.window_fullscreen_why(&w, true, "in the sofa mode");
        }
        self.sofa_body_screen(Screen::Start);
        eprintln!(
            "bitscomp: sofa: on ({}), from workspace {:?}; {} shown",
            way.word(),
            self.sofa.before.as_deref().unwrap_or("-"),
            if self.sofa.front == Front::Program { "the program on Sofa" } else { "the start screen" }
        );
        self.sofa_focus_default();
        // GM-6c: sofa.start_volume, once per start of the mode, only lowering (the thread reads and writes).
        let to = self.sofa.prefs.start_volume.clamp(0, 100) as u8;
        self.sofa_io(crate::sofa_io::Job::Lower(to));
        self.sofa_changed();
    }

    fn sofa_leave(&mut self, way: Way) {
        self.sofa.active = false;
        self.sofa.nav.quick = false;
        self.sofa.confirm = None;
        self.sofa.set.learn = None;
        self.sofa.hover = None;
        self.sofa_search_forget_secret();
        let sofa_i = self.desk.names.iter().position(|n| n == sofa::WORKSPACE);
        let back = self.sofa.before.as_ref().and_then(|b| self.desk.names.iter().position(|n| n == b));
        let to = back.or_else(|| (0..self.ws_count()).find(|i| Some(*i) != sofa_i)).unwrap_or(0);
        self.ws_switch(to);
        eprintln!(
            "bitscomp: sofa: off ({}), workspace {:?} shown; Sofa keeps its {} windows",
            way.word(),
            self.desk.names.get(to).map_or("-", String::as_str),
            sofa_i.map_or(0, |i| self.ws_counts(i).1)
        );
        self.sofa_changed();
    }

    /// A pad came (evdev, or a test pad): with `sofa.start_on_pad` it starts the sofa mode; before settingsd's
    /// values came it waits for them (at most `BOOT_WAIT`, then the defaults decide).
    pub fn sofa_pad_plugged(&mut self) {
        let way = if self.sofa.settings_seen { Way::PadPlug } else { Way::Boot };
        if self.settings_live || self.sofa.settings_seen {
            return self.sofa_switch(way);
        }
        if self.sofa.pending.is_none() {
            self.sofa.pending = Some(crate::pacer::now() + sofa::BOOT_WAIT);
            eprintln!("bitscomp: sofa: a pad is there; sofa.start_on_pad decides once the settings are read (at most {} s)", sofa::BOOT_WAIT.as_secs());
        }
    }

    pub fn sofa_due(&self) -> Option<std::time::Duration> {
        [self.sofa.pending, self.sofa_quick_due(), self.sofa.search.due()].into_iter().flatten().min()
    }

    pub fn sofa_tick(&mut self, now: std::time::Duration) {
        self.sofa_quick_tick(now);
        self.sofa_search_tick(now);
        if self.sofa.pending.is_some_and(|t| now >= t) {
            self.sofa.pending = None;
            eprintln!("bitscomp: sofa: the settings did not come within {} s: the defaults decide for the pad", sofa::BOOT_WAIT.as_secs());
            self.sofa_switch(Way::Boot);
        }
    }

    /// settingsd's values are there (its first list): a waiting pad is decided now.
    pub fn sofa_settings_came(&mut self) {
        self.sofa.settings_seen = true;
        if self.sofa.pending.take().is_some() {
            self.sofa_switch(Way::Boot);
        }
    }

    /// The values of §6 after a settings change; the pads follow `sofa.home_chord`.
    pub fn sofa_apply(&mut self) {
        let p = Prefs::of(&self.settings);
        if p == self.sofa.prefs {
            return;
        }
        eprintln!(
            "bitscomp: sofa: settings start_on_pad={} motion={} sort={} start_volume={} overscan={} home_chord={}",
            p.start_on_pad,
            p.motion,
            p.sort,
            p.start_volume,
            p.overscan,
            if p.chord { "select_start" } else { "aus" }
        );
        for (_, d) in &mut self.pads.list {
            d.pad.chord_off = !p.chord;
        }
        let remap = p.pad_map != self.sofa.prefs.pad_map;
        self.sofa.prefs = p;
        if remap {
            self.pad_remap();
        }
        self.needs_frame = true;
    }

    fn sofa_keys_known(&self) -> bool {
        sofa::KEYS.iter().all(|k| self.setting_keys.iter().any(|(s, _)| s == k))
    }

    /// The keys of §6 into settingsd (owner bitscomp), once per connection, when it does not know them yet. Sent
    /// by the writer thread (B4); a refusal is logged and the defaults (or `ctl set`) apply.
    pub fn sofa_register(&mut self) {
        if !self.settings_live || self.sofa.keys_sent || self.sofa_keys_known() {
            return;
        }
        let Some(w) = self.settings_writer.as_ref() else { return };
        let sent = w.send(crate::desk::DESK_PID, crate::settings_write::Req::Defs { keys: sofa::defs() });
        eprintln!("bitscomp: sofa: keys {} ({})", sofa::KEYS.join(","), if sent { "registering in settingsd" } else { "writer gone" });
        self.sofa.keys_sent = sent;
    }

    /// A new window on the shown workspace: on `Sofa` in the sofa mode it comes to the front, fullscreen.
    pub fn sofa_window_new(&mut self, w: &Window) {
        if !self.sofa.active || self.ws.find(w).is_none() {
            return;
        }
        self.ws.focus_window(w);
        if !self.ws.is_fullscreen(w) {
            self.window_fullscreen_why(w, true, "in the sofa mode");
        }
        let full = self.ws.is_fullscreen(w);
        self.sofa.front = Front::Program;
        self.sofa.nav.quick = false;
        eprintln!("bitscomp: sofa: window {} opens on Sofa in front{}", crate::ctl::win_id(w), if full { ", fullscreen" } else { "" });
        self.sofa_changed();
    }

    /// A window went: the last one on `Sofa` gives the screen back to the sofa shell (a quick menu opened over the
    /// program closes with it: its entries were the program's).
    pub fn sofa_window_gone(&mut self) {
        self.sofa_quick_gone();
        if self.sofa.active && self.sofa.front == Front::Program && self.ws.tiles.is_empty() && self.ws.floating.is_empty() {
            self.sofa.front = Front::Shell;
            self.sofa.nav.quick = false;
            self.sofa.confirm = None;
            eprintln!("bitscomp: sofa: the last window on Sofa went: the start screen shows");
            self.sofa_focus_default();
            self.sofa_changed();
        }
    }

    /// A pad press while the sofa layer listens (pad_app routes Home before; behind a program it never comes here).
    pub fn sofa_pad(&mut self, p: Press) {
        // GM-6d: on the search screen A presses the key, X deletes, Y is space, LB/RB move the cursor, Start is Suchen.
        if self.sofa_search_pad(p) {
            return;
        }
        match p {
            Press::Move(d) => self.sofa_move(d),
            Press::Btn(Btn::A) => self.sofa_activate_focus(),
            Press::Btn(Btn::B) => self.sofa_back(),
            Press::Btn(Btn::Y) => self.sofa_search_open(Target::Search, ""),
            Press::Btn(Btn::Start) => self.sofa_quick_toggle("pad Start"),
            Press::Btn(Btn::LB) => self.sofa_pill_step(-1),
            Press::Btn(Btn::RB) => self.sofa_pill_step(1),
            // GM-6c: X on Einstellungen undoes the newest change made there.
            Press::Btn(Btn::X) if self.sofa.nav.screen == Screen::Einstellungen && !self.sofa.nav.quick && self.sofa.confirm.is_none() => self.sofa_set_undo(),
            // X opens the circle menu on a tile (later); Select does nothing.
            _ => {}
        }
    }

    /// A key while the sofa layer listens; false for the modifiers (they reach the client as before).
    pub fn sofa_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        if crate::library_ui::is_modifier(sym) {
            return false;
        }
        if self.sofa_search_key(sym, mods) {
            return true;
        }
        match sym {
            Keysym::Left | Keysym::KP_Left => self.sofa_move(Dir::Left),
            Keysym::Right | Keysym::KP_Right => self.sofa_move(Dir::Right),
            Keysym::Up | Keysym::KP_Up => self.sofa_move(Dir::Up),
            Keysym::Down | Keysym::KP_Down => self.sofa_move(Dir::Down),
            Keysym::Return | Keysym::KP_Enter => self.sofa_activate_focus(),
            Keysym::Escape => self.sofa_back(),
            Keysym::BackSpace if self.sofa.nav.screen == Screen::Einstellungen && !self.sofa.nav.quick && self.sofa.confirm.is_none() => self.sofa_set_undo(),
            Keysym::Prior if !self.sofa.nav.quick => self.sofa_pill_step(-1),
            Keysym::Next if !self.sofa.nav.quick => self.sofa_pill_step(1),
            _ => match sym.key_char() {
                // A real keyboard typing a letter opens the search with it (§4).
                Some(c) if !c.is_control() && !mods.ctrl && !mods.alt && !mods.logo && !self.sofa.nav.quick && self.sofa.confirm.is_none() => {
                    self.sofa_search_open(Target::Search, "");
                    if self.sofa.nav.screen == Screen::Suche {
                        self.sofa_type(c);
                    }
                }
                _ => {}
            },
        }
        true
    }

    fn sofa_move(&mut self, d: Dir) {
        if self.sofa.set.learn.is_some() || self.sofa_quick_slide(d) || self.sofa_scroll_move(d) {
            return;
        }
        if matches!(d, Dir::Left | Dir::Right) && self.sofa_set_change(if d == Dir::Right { 1 } else { -1 }) {
            return;
        }
        let items = self.sofa_items();
        let Some(from) = self.sofa.focus.as_deref().and_then(|id| focus::index(&items, id)) else {
            return self.sofa_focus_default();
        };
        if let Some(to) = focus::step(&items, from, d) {
            self.sofa.focus = Some(items[to].id.clone());
            self.sofa_follow();
            self.needs_frame = true;
        }
    }

    /// Another screen; not while the quick menu is open (the screen must not change under it: Y, LB/RB, Bild auf/ab
    /// and the search button wait until it is closed).
    pub(crate) fn sofa_go(&mut self, s: Screen) {
        if self.sofa.nav.screen == s || self.sofa.nav.quick {
            return;
        }
        self.sofa.nav.go(s);
        self.sofa_body_screen(s);
        eprintln!("bitscomp: sofa: screen {}", s.word());
        let pill = s.pill();
        // A pill keeps the focus on itself; another way starts on the screen's own default.
        if !(pill.is_some() && self.sofa.focus.as_deref().is_some_and(|f| f.starts_with("pill:"))) {
            self.sofa_focus_default();
        } else {
            self.sofa.focus = pill;
        }
        self.needs_frame = true;
    }

    fn sofa_pill_step(&mut self, by: i32) {
        // GM-6b: in the library LB/RB, Bild auf/ab and the wheel change the collection.
        if self.sofa.nav.quick || self.sofa.confirm.is_some() || self.sofa_lbrb(by) || self.sofa_set_lbrb(by) {
            return;
        }
        if let Some(s) = self.sofa.nav.pill_step(by) {
            self.sofa_go(s);
        }
    }

    fn sofa_quick_toggle(&mut self, why: &str) {
        if self.sofa.nav.quick {
            return self.sofa_quick_close(why);
        }
        self.sofa.nav.quick = true;
        eprintln!("bitscomp: sofa: quick menu opened ({why})");
        self.sofa_quick_opened();
        self.sofa_changed();
    }

    fn sofa_back(&mut self) {
        // GM-6c: a question is answered Nein, the learn screen closes, before anything else.
        if self.sofa.confirm.is_some() {
            return self.sofa_confirm(false);
        }
        if self.sofa_learn_cancel() {
            return;
        }
        let items = self.sofa_items();
        let area = self.sofa.focus.as_deref().and_then(|id| focus::index(&items, id)).map(|i| items[i].area);
        match self.sofa.nav.back(area) {
            Back::Quick => {
                eprintln!("bitscomp: sofa: quick menu closed (back)");
                self.sofa_quick_left();
                self.sofa_changed();
            }
            Back::Screen(s) => {
                eprintln!("bitscomp: sofa: back to {}", s.word());
                self.sofa_body_screen(s);
                self.sofa_focus_default();
            }
            Back::Column => {
                // GM-6c: back to the section shown (its rows were the focus's).
                let own = format!("section:{}", self.sofa.set.section);
                self.sofa.focus = items.iter().find(|i| i.id == own).or_else(|| items.iter().find(|i| i.area == Area::Side)).map(|i| i.id.clone());
                self.needs_frame = true;
            }
            Back::Stay => {}
        }
    }

    fn sofa_activate_focus(&mut self) {
        if let Some(id) = self.sofa.focus.clone() {
            self.sofa_activate(&id);
        }
    }

    /// A (Enter, a click) on element `id`, or a click on a footer hint.
    pub(crate) fn sofa_activate(&mut self, id: &str) {
        if let Some(w) = id.strip_prefix("pill:") {
            if let Some(s) = Screen::of_word(w) {
                self.sofa.focus = Some(id.to_string());
                self.sofa_go(s);
            }
            return;
        }
        match id {
            "search" => self.sofa_search_open(Target::Search, ""),
            "menu" => self.sofa_quick_toggle("Menü"),
            "hint:a" => self.sofa_activate_focus(),
            "hint:b" => self.sofa_back(),
            "hint:y" => self.sofa_search_open(Target::Search, ""),
            "hint:home" => self.sofa_switch(Way::Desktop),
            "hint:lbrb" => self.sofa_pill_step(1),
            "hint:lr" => self.sofa_move(Dir::Right),
            "hint:x" => self.sofa_set_undo(),
            "c:yes" => self.sofa_confirm(true),
            "c:no" => self.sofa_confirm(false),
            other => {
                if !self.sofa_search_activate(other) && !self.sofa_quick_activate(other) && !self.sofa_set_activate(other) && !self.sofa_body_activate(other) {
                    eprintln!("bitscomp: sofa: {other} has nothing to do yet (later GM-6 items)");
                }
            }
        }
    }

    /// The pointer in board px of the sofa layout.
    fn sofa_pointer(&self) -> (f64, f64) {
        let s = self.theme.scale;
        self.sofa_frame().board_point(self.pointer.x * s, self.pointer.y * s)
    }

    /// The hover sets the focus (§4); over a hint it lights the hint.
    pub fn sofa_motion(&mut self) {
        let (x, y) = self.sofa_pointer();
        let items = self.sofa_items();
        if let Some(i) = focus::at(&items, x, y)
            && self.sofa.focus.as_deref() != Some(items[i].id.as_str())
        {
            self.sofa.focus = Some(items[i].id.clone());
            self.sofa_follow();
            self.needs_frame = true;
        }
        let hint = self.sofa_hints().into_iter().find(|p| p.rect.contains(x, y)).map(|p| p.hint.id.to_string());
        if hint != self.sofa.hover {
            self.sofa.hover = hint;
            self.needs_frame = true;
        }
    }

    pub fn sofa_press(&mut self, code: u32) {
        use crate::pointer::{BTN_LEFT, BTN_RIGHT};
        let (x, y) = self.sofa_pointer();
        if code == BTN_RIGHT {
            return self.sofa_back();
        }
        if code != BTN_LEFT {
            return;
        }
        if self.sofa.nav.quick {
            let (bw, _) = self.sofa_frame().board();
            if x < (bw - lay::QUICK_W) as f64 {
                return self.sofa_quick_close("click beside it");
            }
        }
        if self.sofa.set.learn.is_some() {
            return;
        }
        let items = self.sofa_items();
        if let Some(i) = focus::at(&items, x, y) {
            let id = items[i].id.clone();
            self.sofa.focus = Some(id.clone());
            if id == "q:volume" {
                return self.sofa_quick_slide_to(x);
            }
            if let Some(n) = id.strip_prefix("row:").and_then(|n| n.parse::<usize>().ok())
                && self.sofa_set_click(n, x, y)
            {
                return;
            }
            return self.sofa_click(&id);
        }
        if let Some(p) = self.sofa_hints().into_iter().find(|p| p.rect.contains(x, y)) {
            self.sofa_activate(p.hint.id);
        }
    }

    /// The wheel: the previous/next header pill (LB/RB), in the library the collection (GM-6b).
    pub fn sofa_wheel(&mut self, steps: f64) {
        if steps != 0.0 && !self.sofa.nav.quick {
            self.sofa_pill_step(if steps > 0.0 { 1 } else { -1 });
        }
    }

    /// `ctl sofa focus WORD`: a move, a button as the pad's (injected input: no dialog is answered by it), or an id.
    fn sofa_focus_ctl(&mut self, w: &str) -> Result<(), String> {
        let press = match w {
            "up" => Press::Move(Dir::Up),
            "down" => Press::Move(Dir::Down),
            "left" => Press::Move(Dir::Left),
            "right" => Press::Move(Dir::Right),
            "a" => Press::Btn(Btn::A),
            "b" => Press::Btn(Btn::B),
            "x" => Press::Btn(Btn::X),
            "y" => Press::Btn(Btn::Y),
            "start" => Press::Btn(Btn::Start),
            "lb" => Press::Btn(Btn::LB),
            "rb" => Press::Btn(Btn::RB),
            id => {
                if focus::index(&self.sofa_items(), id).is_none() {
                    return Err(format!("sofa focus: no element {id} on this screen"));
                }
                self.sofa.focus = Some(id.to_string());
                self.sofa_follow();
                self.needs_frame = true;
                return Ok(());
            }
        };
        if !self.sofa.listens() {
            return Err("sofa focus: the sofa screens are not shown".into());
        }
        if self.install.shown() {
            return Err("sofa focus: the installer's question is shown".into());
        }
        // A ctl press is injected input whatever came before it (GM-6w: only a real press may ask init to go down).
        let keep = std::mem::replace(&mut self.input_origin, crate::input::Origin::Injected);
        self.sofa_pad(press);
        self.input_origin = keep;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::Block;

    #[test]
    fn the_network_word_comes_from_the_status_block() {
        let b = |v: &str| vec![Block { label: "Netz".into(), value: v.into() }];
        assert_eq!(net_word(&b("Kabel"), None), "Kabel");
        assert_eq!(net_word(&b("WLAN"), Some("Wohnzimmer")), "WLAN Wohnzimmer");
        assert_eq!(net_word(&b("WLAN"), Some(" ")), "WLAN");
        assert_eq!(net_word(&b("getrennt"), None), "Kein Netz");
        assert_eq!(net_word(&[], None), "Kein Netz");
    }
}
