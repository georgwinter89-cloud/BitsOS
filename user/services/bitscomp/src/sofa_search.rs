//! GM-6d: the sofa mode's search and on-screen keyboard (plan/spec/sofa-mode.md §2.6), the pure parts: the text
//! field edited by character (an umlaut or ß is one character, the cursor counts characters), what each key of the
//! German keyboard does on its two levels, the URL encoding of the Brave search address, and the hold-repeat of a
//! key (500 ms, then every 33 ms: IN-3's numbers). Host-tested in `sofa_search_tests.rs`; sofa_search_app.rs wires
//! them into bitscomp, sofa_search_draw.rs draws them.

use std::time::Duration;

use smithay::input::keyboard::{Keycode, Keysym, ModifiersState};

use crate::sofa::TYPED_MAX;
use crate::sofa_layout::Level;
use crate::sofa_model::Hit;

pub const REPEAT_FIRST: Duration = Duration::from_millis(500);
pub const REPEAT_EVERY: Duration = Duration::from_millis(33);
/// Brave's search address; the text follows URL-encoded (§3.2).
pub const BRAVE_SEARCH: &str = "https://search.brave.com/search?q=";

/// One line of text and its cursor (in characters), at most `TYPED_MAX` characters (one field's input, R1).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    pub cursor: usize,
    /// A character was dropped at the bound (said once until something is deleted).
    pub full: bool,
}

impl Field {
    /// A field holding `text` (cut to the bound), the cursor at its end.
    pub fn of(text: &str) -> Field {
        let text: String = text.chars().take(TYPED_MAX).collect();
        Field { cursor: text.chars().count(), text, full: false }
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte(&self, at: usize) -> usize {
        self.text.char_indices().nth(at).map_or(self.text.len(), |(b, _)| b)
    }

    /// `c` at the cursor; false at the bound (`full` is set).
    pub fn insert(&mut self, c: char) -> bool {
        if self.len() >= TYPED_MAX {
            self.full = true;
            return false;
        }
        let b = self.byte(self.cursor);
        self.text.insert(b, c);
        self.cursor += 1;
        true
    }

    /// The character before the cursor goes (Löschen, X, the Rücktaste).
    pub fn backspace(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        let b = self.byte(self.cursor);
        self.text.remove(b);
        self.full = false;
        true
    }

    /// The character after the cursor goes (Entf).
    pub fn delete(&mut self) -> bool {
        if self.cursor >= self.len() {
            return false;
        }
        let b = self.byte(self.cursor);
        self.text.remove(b);
        self.full = false;
        true
    }

    /// The cursor `by` characters on, inside the text.
    pub fn step(&mut self, by: i32) {
        self.cursor = (self.cursor as i64 + by as i64).clamp(0, self.len() as i64) as usize;
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.len();
    }
}

/// What a key of the on-screen keyboard does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Delete,
    Shift,
    /// `&123` and `abc`: the other level.
    Level,
    Space,
    /// Suchen (or Fertig on a settings field).
    Enter,
}

/// The key labelled `label` (sofa_layout's `key_rows`).
pub fn key(label: &str) -> Option<Key> {
    Some(match label {
        "⌫ Löschen" => Key::Delete,
        "⇧ Groß" => Key::Shift,
        "&123" | "abc" => Key::Level,
        "Leerzeichen" => Key::Space,
        "Suchen" | "Fertig" => Key::Enter,
        s => {
            let mut it = s.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None;
            }
            Key::Char(c)
        }
    })
}

/// The character a letter key types: with ⇧ Groß its capital (Ä Ö Ü too); ß and the others have none of one
/// character and stay as they are.
pub fn shifted(c: char, shift: bool) -> char {
    if !shift {
        return c;
    }
    let mut up = c.to_uppercase();
    match (up.next(), up.next()) {
        (Some(u), None) => u,
        _ => c,
    }
}

/// `s` URL-encoded for a query (RFC 3986): letters, digits and `-._~` stay, every other byte of its UTF-8 is
/// `%XX` (a space is `%20`).
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The Brave search hit's address for the typed text (the only web address the sofa mode builds, §2.6).
pub fn brave_url(text: &str) -> String {
    format!("{BRAVE_SEARCH}{}", url_encode(text.trim()))
}

/// A held key's repeat: first after `REPEAT_FIRST`, then every `REPEAT_EVERY`; a late tick repeats once, not in
/// a burst.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Repeat {
    pub next: Duration,
}

impl Repeat {
    pub fn start(now: Duration) -> Repeat {
        Repeat { next: now + REPEAT_FIRST }
    }

    /// Whether the key repeats at `now` (and when next).
    pub fn fire(&mut self, now: Duration) -> bool {
        if now < self.next {
            return false;
        }
        let n = self.next + REPEAT_EVERY;
        self.next = if n <= now { now + REPEAT_EVERY } else { n };
        true
    }
}

/// A real key held on the search screen (bitscomp repeats it: no client sees it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyHeld {
    pub code: Keycode,
    pub sym: Keysym,
    pub mods: ModifiersState,
    pub rep: Repeat,
}

/// What the keyboard types into.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Target {
    /// The search (hits under the field).
    #[default]
    Search,
    /// A settings row of Einstellungen › Netz (`net.wifi.ssid`, `net.wifi.psk`); a secret shows as dots and is
    /// never read back.
    Setting { key: String, title: String, secret: bool },
}

impl Target {
    pub fn secret(&self) -> bool {
        matches!(self, Target::Setting { secret: true, .. })
    }

    /// The last key's word.
    pub fn enter(&self) -> &'static str {
        if *self == Target::Search { "Suchen" } else { "Fertig" }
    }
}

/// The search screen's state (`Sofa::search`).
#[derive(Default)]
pub struct Search {
    pub field: Field,
    pub level: Level,
    /// ⇧ Groß for the next letter.
    pub shift: bool,
    pub target: Target,
    /// The program matches of the field's text, best first (computed when the text changes, from the cached
    /// library index: B4).
    pub hits: Vec<Hit>,
    /// The text `hits` belong to.
    pub hits_for: Option<String>,
    /// The second row's first card shown.
    pub first: usize,
    /// A refusal or a fallback said on the screen.
    pub note: Option<String>,
    /// A held to repeat: the key's id and its timer.
    pub pad: Option<(String, Repeat)>,
    pub held: Option<KeyHeld>,
}

impl Search {
    /// Whether the hit cards show (the field holds more than spaces, on the search).
    pub fn shows_hits(&self) -> bool {
        self.target == Target::Search && !self.field.text.trim().is_empty()
    }

    /// The second row's matches (after the best).
    pub fn more(&self) -> usize {
        if self.shows_hits() { self.hits.len().saturating_sub(1) } else { 0 }
    }

    /// `first` moved so the second row's card `n` (0-based) is among the three shown.
    pub fn follow(&mut self, n: usize) {
        if n < self.first {
            self.first = n;
        } else if n >= self.first + 3 {
            self.first = n - 2;
        }
    }

    /// The timer that wakes the loop next.
    pub fn due(&self) -> Option<Duration> {
        [self.pad.as_ref().map(|(_, r)| r.next), self.held.map(|h| h.rep.next)].into_iter().flatten().min()
    }
}

#[cfg(test)]
#[path = "sofa_search_tests.rs"]
mod tests;
