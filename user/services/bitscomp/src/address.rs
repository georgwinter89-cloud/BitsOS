//! The address bar (plan/spec/desktop.md §1a, WY-5): it shows the selected tab's address; a click (or Ctrl+Alt+L)
//! makes it editable, and Enter opens what BitsOS can open today: `programm/terminal` (also `terminal`, `foot`,
//! `app/terminal`) starts foot, `einstellungen/...` opens bitscomp's settings page (`page`, WY-5b), a web
//! address is noted as not available yet. Enter and Alt+Enter open in a new tab of the focused stack (a tab cannot
//! navigate yet: there is no address layer below it), Ctrl+Enter in a new tile.

use std::time::Duration;

use smithay::input::keyboard::{Keycode, Keysym, ModifiersState};

use crate::state::{App, KEY_REPEAT};

/// A key held while the address bar is edited. No client sees these keys, and Wayland leaves the repeat to the
/// client (the evdev and winit back ends drop the kernel's soft repeat), so bitscomp repeats them itself, at the
/// keyboard's repeat_info (KEY_REPEAT): BackSpace held deletes on, as in every other text field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub code: Keycode,
    pub sym: Keysym,
    pub mods: ModifiersState,
    /// When it repeats next (`pacer` time).
    pub at: Duration,
}

/// Enter and Esc end the editing, modifiers do nothing by themselves: these never repeat.
pub fn repeats(sym: Keysym) -> bool {
    !sym.is_modifier_key() && !matches!(sym, Keysym::Return | Keysym::KP_Enter | Keysym::Escape)
}

pub fn repeat_every() -> Duration {
    Duration::from_secs(1) / KEY_REPEAT.rate.max(1) as u32
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
    /// The suggestion chosen with the arrow keys.
    pub pick: Option<usize>,
    /// The tile whose own bar is edited (`theme.layout.address_bar = tile`); `None`: the top bar.
    pub tile: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Open {
    Terminal,
    /// A key prefix of the settings registry.
    Settings(String),
    Web(String),
    Unknown(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Tab,
    Tile,
}

/// German address words; the English short forms resolve to the same thing (desktop.md §1a).
pub fn normalise(addr: &str) -> String {
    let a = addr.trim().to_lowercase();
    for (en, de) in [("app/", "programm/"), ("settings/", "einstellungen/"), ("docs/", "dokumente/"), ("device/", "geraet/"), ("context/", "kontext/")] {
        if let Some(rest) = a.strip_prefix(en) {
            return format!("{de}{rest}");
        }
    }
    if a == "settings" { "einstellungen".into() } else { a }
}

pub fn resolve(addr: &str) -> Open {
    let a = normalise(addr);
    match a.as_str() {
        "terminal" | "foot" | "programm/terminal" | "programm/foot" => return Open::Terminal,
        "einstellungen" => return Open::Settings(String::new()),
        _ => {}
    }
    if let Some(rest) = a.strip_prefix("einstellungen/") {
        // German words for the keys (`aussehen/akzent` = theme.accent); other paths are key prefixes.
        return Open::Settings(crate::page::prefix_of(rest));
    }
    let web = a.starts_with("http://") || a.starts_with("https://") || a.starts_with("www.")
        || (!a.contains('/') && !a.contains(' ') && a.contains('.'))
        || a.split('/').next().is_some_and(|h| h.contains('.') && !h.starts_with('.'));
    if web { Open::Web(addr.trim().into()) } else { Open::Unknown(addr.trim().into()) }
}

/// The address of a window: its program (`programm/terminal` for foot).
pub fn of_window(app_id: &str) -> String {
    let id = app_id.trim().to_lowercase();
    match id.as_str() {
        "" => "programm/fenster".into(),
        "foot" | "footclient" => "programm/terminal".into(),
        _ => format!("programm/{id}"),
    }
}

impl Edit {
    fn byte(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map_or(self.text.len(), |(b, _)| b)
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn insert(&mut self, c: char) {
        let b = self.byte(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
        self.pick = None;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let b = self.byte(self.cursor - 1);
            self.text.remove(b);
            self.cursor -= 1;
            self.pick = None;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.len() {
            let b = self.byte(self.cursor);
            self.text.remove(b);
            self.pick = None;
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }

    /// The text before the cursor (where the cursor line is drawn).
    pub fn before(&self) -> &str {
        &self.text[..self.byte(self.cursor)]
    }
}

impl App {
    /// The address the top bar shows: the selected tab's (the focused window's).
    pub fn shown_address(&self) -> Option<String> {
        self.ws.focused().map(|w| self.address_of(w))
    }

    /// The address of tile `t`'s shown page.
    pub fn tile_address(&self, t: usize) -> Option<String> {
        let tile = self.ws.tiles.get(t)?;
        Some(self.address_of(tile.stack.get(tile.top)?))
    }

    pub fn address_begin(&mut self, tile: Option<usize>) {
        let text = match tile {
            Some(t) => self.tile_address(t),
            None if self.theme.layout.address_top => self.shown_address(),
            None => None,
        }
        .unwrap_or_default();
        let cursor = text.chars().count();
        self.page_edit_end();
        self.desk.edit = Some(Edit { text, cursor, pick: None, tile });
        self.menu = None;
        self.chrome_dirty();
    }

    pub fn address_end(&mut self) {
        self.desk.held = None;
        if self.desk.edit.take().is_some() {
            self.chrome_dirty();
        }
    }

    /// A key went down while editing: it repeats after the delay while held (a new key replaces it).
    pub fn address_hold(&mut self, code: Keycode, sym: Keysym, mods: ModifiersState) {
        if (self.desk.edit.is_some() || self.page_edit_pid().is_some()) && repeats(sym) {
            let at = crate::pacer::now() + Duration::from_millis(KEY_REPEAT.delay_ms.max(0) as u64);
            self.desk.held = Some(Held { code, sym, mods, at });
        }
    }

    pub fn address_release(&mut self, code: Keycode) {
        if self.desk.held.is_some_and(|h| h.code == code) {
            self.desk.held = None;
        }
    }

    pub fn repeat_due(&self) -> Option<Duration> {
        self.desk.held.map(|h| h.at)
    }

    /// Repeats the held key when due; a stalled loop does not catch up in a burst.
    pub fn repeat_tick(&mut self, now: Duration) {
        let Some(mut h) = self.desk.held else { return };
        let page = self.page_edit_pid();
        if self.desk.edit.is_none() && page.is_none() {
            self.desk.held = None;
            return;
        }
        if now < h.at {
            return;
        }
        h.at = (h.at + repeat_every()).max(now);
        self.desk.held = Some(h);
        match page {
            Some(pid) if self.desk.edit.is_none() => self.page_key(pid, h.sym, &h.mods),
            _ => self.address_key(h.sym, &h.mods),
        }
    }

    /// A key while the address bar is edited; every key stays with bitscomp then.
    pub fn address_key(&mut self, sym: Keysym, mods: &ModifiersState) {
        let rows = self.suggestions();
        let n = rows.len();
        let Some(e) = self.desk.edit.as_mut() else { return };
        match sym {
            Keysym::Escape => return self.address_end(),
            Keysym::Return | Keysym::KP_Enter => {
                let text = match e.pick {
                    Some(i) => rows.get(i).map(|r| r.addr.clone()).unwrap_or_else(|| e.text.clone()),
                    None => e.text.clone(),
                };
                let tile = e.tile;
                let w = if mods.ctrl { Where::Tile } else { Where::Tab };
                self.address_end();
                self.open_address(&text, w, tile);
                return;
            }
            Keysym::BackSpace => e.backspace(),
            Keysym::Delete | Keysym::KP_Delete => e.delete(),
            Keysym::Left | Keysym::KP_Left => e.left(),
            Keysym::Right | Keysym::KP_Right => e.right(),
            Keysym::Home | Keysym::KP_Home => e.home(),
            Keysym::End | Keysym::KP_End => e.end(),
            Keysym::Down | Keysym::KP_Down if n > 0 => e.pick = Some(e.pick.map_or(0, |p| (p + 1) % n)),
            Keysym::Up | Keysym::KP_Up if n > 0 => e.pick = Some(e.pick.map_or(n - 1, |p| (p + n - 1) % n)),
            _ => match sym.key_char() {
                Some(c) if !c.is_control() && !mods.ctrl && !mods.logo => e.insert(c),
                _ => return,
            },
        }
        self.chrome_dirty();
    }

    /// Opens `addr` (see the module comment); `tile` is the tile whose own bar it came from.
    pub fn open_address(&mut self, addr: &str, w: Where, tile: Option<usize>) {
        let addr = addr.trim();
        if addr.is_empty() {
            return;
        }
        match resolve(addr) {
            Open::Terminal => self.open_program(crate::terminal::TERMINAL.iter().map(|s| s.to_string()).collect(), w, tile),
            // WY-5b: bitscomp's own settings page (its prefix comes from the address again).
            Open::Settings(_) => self.open_page(&normalise(addr), w, tile),
            Open::Web(a) => self.notice(format!("Webseiten kann BitsOS noch nicht öffnen: {a}")),
            Open::Unknown(a) => self.notice(format!("Unbekannte Adresse: {a}")),
        }
        if !matches!(resolve(addr), Open::Unknown(_) | Open::Web(_)) {
            self.recent_add(&normalise(addr));
        }
    }

    /// Starts a program whose first window goes into a new tab of the focused (or given) tile, or a new tile.
    pub fn open_program(&mut self, argv: Vec<String>, w: Where, tile: Option<usize>) {
        let target = match (w, tile) {
            (Where::Tile, _) => None,
            (Where::Tab, Some(t)) => self.ws.tiles.get(t).map(|t| t.stack[t.top].clone()),
            (Where::Tab, None) => self.ws.focused().cloned(),
        };
        let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
        if let Some(pid) = self.spawn(&refs) {
            self.pending_open.push((pid, target, argv));
        }
        self.desk.start_forced = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_keys_repeat_enter_and_modifiers_do_not() {
        for k in [Keysym::BackSpace, Keysym::Delete, Keysym::Left, Keysym::Right, Keysym::a, Keysym::adiaeresis] {
            assert!(repeats(k), "{k:?}");
        }
        for k in [Keysym::Return, Keysym::KP_Enter, Keysym::Escape, Keysym::Shift_L, Keysym::Control_L, Keysym::ISO_Level3_Shift] {
            assert!(!repeats(k), "{k:?}");
        }
        assert_eq!(repeat_every(), Duration::from_secs(1) / KEY_REPEAT.rate as u32);
    }

    #[test]
    fn addresses_resolve_to_what_can_be_opened() {
        for a in ["programm/terminal", "terminal", "foot", "App/Terminal", " programm/foot "] {
            assert_eq!(resolve(a), Open::Terminal, "{a}");
        }
        assert_eq!(resolve("einstellungen/aussehen"), Open::Settings("theme".into()));
        assert_eq!(resolve("settings/aussehen/accent"), Open::Settings("theme.accent".into()));
        assert_eq!(resolve("einstellungen/theme.layout"), Open::Settings("theme.layout".into()));
        assert_eq!(resolve("einstellungen"), Open::Settings(String::new()));
        assert_eq!(resolve("einstellungen/aussehen/akzent"), Open::Settings("theme.accent".into()));
        assert_eq!(resolve("Einstellungen/Aufbau/Statusleiste"), Open::Settings("theme.layout.statusbar".into()));
        assert_eq!(resolve("settings/favoriten"), Open::Settings("desktop.favourites".into()));
        for a in ["https://www.elster.de", "youtube.com", "www.example.org", "example.org/a/b"] {
            assert_eq!(resolve(a), Open::Web(a.into()), "{a}");
        }
        assert_eq!(resolve("dokumente/rechnung-oktober"), Open::Unknown("dokumente/rechnung-oktober".into()));
        assert_eq!(of_window("foot"), "programm/terminal");
        assert_eq!(of_window("Brave-Browser"), "programm/brave-browser");
    }

    #[test]
    fn editing_handles_umlauts_and_the_cursor() {
        let mut e = Edit { text: "grüße".into(), cursor: 5, pick: None, tile: None };
        e.backspace();
        assert_eq!(e.text, "grüß");
        e.home();
        e.right();
        e.right();
        e.insert('X');
        assert_eq!(e.text, "grXüß");
        assert_eq!(e.before(), "grX");
        e.delete();
        assert_eq!(e.text, "grXß");
        e.end();
        e.insert('€');
        assert_eq!(e.text, "grXß€");
    }
}
