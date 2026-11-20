//! EINGABE/EINGABE2: how the desktop writes a `type` step's text into a foreign program's field that offers no
//! EditableText, or presses Enter in it (`abschicken`). atspid already gave that field the focus and saw it as
//! the one editable element of its window with the focus (proof i). Without the desktop: one look at a time, what
//! to do next. First the program's own text input (text-input-v3): its fresh state after the focus change must
//! show the same field (proof ii: the caret inside the field's place, the surrounding text equal to the field's
//! text); then the old content goes (the whole surrounding text deleted with the commit, or all of it selected
//! through AT-SPI first) and the text replaces it. If no text input is enabled for that window within a second, a
//! paste only that window sees (`desk_paste`), after the field's text was selected; never letter-by-letter keys.
//! Right before the text or the key goes out atspid is asked once more whether the field still has the keyboard.
//! It stops, writing nothing, when Georg's own hand types, clicks or moves the mouse, when the keyboard is no
//! longer that window's, the field lost the focus, the text input shows another field, or the field could not be
//! emptied; once the program has taken the text, it is in whatever follows.

use std::time::{Duration, Instant};

use crate::text_in::Field;

/// How long the window's text input may take to be enabled for the focused field.
pub const INPUT_WAIT: Duration = Duration::from_millis(1000);
/// How long atspid may take to say whether the field still has the focus.
pub const CHECK_WAIT: Duration = Duration::from_millis(3000);
/// How long the program may take to ask for the paste after Ctrl+V.
pub const PASTE_WAIT: Duration = Duration::from_millis(2000);
/// The program's moment to show the text before the window is read again.
pub const SETTLE: Duration = Duration::from_millis(150);
/// The most a step may write.
pub const MAX_CHARS: usize = 2000;

pub const HAND: &str = "Georg hat selbst getippt, geklickt oder die Maus bewegt; Durchblick hat nichts geschrieben.";
pub const MOVED: &str = "Das Fenster oder sein Feld hat die Tastatur nicht mehr; Durchblick hat nichts geschrieben.";
pub const SECRET: &str = "Das Feld fragt nach einem Passwort oder einer PIN; die gibt nur Georg ein. Es wurde nichts geschrieben.";
/// After Ctrl+V: the program did not ask for the text, but it may have pasted something else (the field shows it).
pub const TIMEOUT: &str = "Das Programm hat den Text nicht angenommen; ob es etwas anderes eingefügt hat, zeigt das Feld.";
pub const OWN: &str = "Das Programm hält selbst die Zwischenablage (Georg hat dort etwas kopiert) und würde das einfügen; es wurde nichts geschrieben.";
pub const SEL_CHANGED: &str = "Die Zwischenablage hat sich beim Einfügen geändert; ob das Programm etwas eingefügt hat, zeigt das Feld.";
pub const NO_WAY: &str = "Das Programm bietet keinen Weg, Text hineinzuschreiben; es wurde nichts geschrieben.";
pub const TERMINAL: &str = "In ein Terminal schreibt Durchblick nicht; es wurde nichts geschrieben.";
pub const HAND_STOP: &str = "Georg hat angehalten; es wurde nichts geschrieben.";
pub const UNCHECKED: &str = "Durchblick konnte nicht prüfen, ob das Feld noch dran ist; es wurde nichts geschrieben.";
pub const BUSY: &str = "Durchblick schreibt gerade schon etwas; ein Schritt nach dem anderen.";
pub const ELSEWHERE: &str = "Die Tastatur ist in einem anderen Feld als dem gewünschten; Durchblick hat nichts geschrieben.";
pub const UNPROVEN: &str = "Das Programm zeigt nicht, welches Feld die Tastatur hat; Durchblick hat nichts geschrieben.";
pub const NOT_EMPTIED: &str = "Das Feld ließ sich nicht leeren (sein alter Text bliebe stehen); Durchblick hat nichts geschrieben.";
pub const NO_KEY: &str = "Das Fenster oder sein Feld hat die Tastatur nicht mehr; Durchblick hat nichts abgeschickt.";
/// The caret may lie this far (logical px) outside the field's place (borders, rounding).
const SLACK: i32 = 4;
/// text-input-v3 sends at most 4000 bytes of surrounding text: longer may be cut, so it is not the whole value.
const WHOLE_MAX: usize = 3800;
/// A cut window of a long value proves the field by its text only when it is this long (a few words match anywhere).
const PART_MIN: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    TextInput,
    Paste,
    /// `abschicken`: one Enter press and release.
    Enter,
}

impl Route {
    pub fn word(self) -> &'static str {
        match self {
            Route::TextInput => "text-input",
            Route::Paste => "paste",
            Route::Enter => "enter",
        }
    }
}

/// What the program's text input said since the focus change (surface-local logical px; byte offsets).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snap {
    /// The text around the cursor, the cursor and the selection's other end, in UTF-8 bytes.
    pub surrounding: Option<(String, u32, u32)>,
    pub caret: Option<[i32; 4]>,
}

/// How the old content goes before the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Replace {
    /// Nothing to remove.
    Empty,
    /// delete_surrounding_text: bytes before and after the cursor (the selection, if any, is replaced by the commit).
    Delete(u32, u32),
    /// All of it selected through AT-SPI first.
    Select,
}

/// Proof (ii) at one look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proof {
    /// No fresh state yet (or one that says neither where the caret is nor what the field holds).
    Wait,
    Here(Replace),
    /// The state is another field's.
    Elsewhere,
}

fn inside(caret: [i32; 4], field: [i32; 4]) -> bool {
    let [cx, cy, cw, ch] = caret;
    let [fx, fy, fw, fh] = field;
    fw > 0 && fh > 0 && cx >= fx - SLACK && cy >= fy - SLACK && cx + cw.max(0) <= fx + fw + SLACK && cy + ch.max(0) <= fy + fh + SLACK
}

/// How the old content of a field holding `value` (None: unread) goes when nothing else is known.
fn replace_blind(value: Option<&str>) -> Replace {
    if value == Some("") { Replace::Empty } else { Replace::Select }
}

/// Proof (ii): the text input's fresh state against the field (its place on the surface, its text as read).
/// `field`: where the field lies in the program's coordinates (the surface's, and the window geometry's: which of
/// the two Chromium counts the caret in is not known here; both lie a shadow's width apart, fields much further).
/// `others`: what the window's other fields hold. A caret outside the field or another text says another field; a
/// caret inside proves it; without a caret the text alone proves it only when it could be no other field's (not
/// empty, no other field holding the same, a long value's cut window long enough), else the wait goes on.
pub fn prove(snap: Option<&Snap>, field: &[[i32; 4]], value: Option<&str>, others: &[&str]) -> Proof {
    let Some(s) = snap else { return Proof::Wait };
    let caret_ok = match s.caret {
        Some(c) if !field.is_empty() => Some(field.iter().any(|f| inside(c, *f))),
        _ => None,
    };
    let text_ok = match (&s.surrounding, value) {
        (Some((t, ..)), Some(v)) if v.len() <= WHOLE_MAX => Some(t == v),
        (Some((t, ..)), Some(v)) => Some(v.contains(t.as_str())),
        _ => None,
    };
    if caret_ok == Some(false) || text_ok == Some(false) {
        return Proof::Elsewhere;
    }
    if caret_ok.is_none() {
        let telling = match (&s.surrounding, value) {
            (Some((t, ..)), Some(v)) if v.len() <= WHOLE_MAX => !t.is_empty() && !others.contains(&t.as_str()),
            (Some((t, ..)), Some(_)) => t.len() >= PART_MIN,
            _ => false,
        };
        if text_ok != Some(true) || !telling {
            return Proof::Wait;
        }
    }
    let whole = |t: &str| value.is_some_and(|v| v == t && v.len() <= WHOLE_MAX);
    Proof::Here(match &s.surrounding {
        Some((t, ..)) if t.is_empty() && value == Some("") => Replace::Empty,
        Some((t, cursor, anchor)) if whole(t) => {
            let (lo, hi) = (*cursor.min(anchor), *cursor.max(anchor));
            Replace::Delete(lo.min(t.len() as u32), (t.len() as u32).saturating_sub(hi))
        }
        _ => replace_blind(value),
    })
}

/// What the desktop sees at one look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The keyboard is that window's surface (and the window is still there).
    pub focus_here: bool,
    pub field: Field,
    /// Proof (ii) from the window's text input now.
    pub proof: Proof,
    /// Georg's own key, click or mouse move since the step started.
    pub hand: bool,
    /// The program asked for the private paste.
    pub served: bool,
    /// atspid's word whether the field still has the keyboard (None: not asked or not answered yet).
    pub focused: Option<bool>,
    /// atspid's word whether all of the field's text is selected (None: not asked or not answered yet).
    pub selected: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Do {
    /// The text into the enabled text input, the old content removed as said.
    Commit(Replace),
    /// Ask atspid to select all of the field's text.
    Select,
    /// Ask atspid whether the field still has the keyboard.
    Check,
    /// The private paste offered to that program alone.
    Offer,
    /// Ctrl+V into that window.
    PasteKey,
    /// Enter into that window.
    EnterKey,
    /// That program gets Georg's real selection back.
    Restore,
    End(Result<Route, String>),
}

/// What a step wants: a text written, or Enter pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Enter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for the text input's proof (or a second without one: the paste or the bare key).
    Input,
    /// atspid selects the field's text, then the route goes on.
    Select(Route),
    /// atspid's word before the text or the key goes out.
    Check(Route, Replace),
    Paste,
    Settle(Route),
    Over,
}

#[derive(Debug)]
pub struct Write {
    kind: Kind,
    /// The field's text as the list showed it (None: unread).
    value: Option<String>,
    phase: Phase,
    since: Instant,
    offered: bool,
    /// The last proof seen while waiting (said when the wait ends).
    last: Proof,
    /// The program had an enabled text input there when the step began: it must show the field (no paste instead).
    had_input: bool,
}

impl Write {
    pub fn new(now: Instant, kind: Kind, value: Option<String>, had_input: bool) -> Write {
        Write { kind, value, phase: Phase::Input, since: now, offered: false, last: Proof::Wait, had_input }
    }

    #[cfg(test)]
    pub fn over(&self) -> bool {
        self.phase == Phase::Over
    }

    fn end(&mut self, r: Result<Route, String>) -> Vec<Do> {
        self.phase = Phase::Over;
        let mut v = Vec::new();
        if std::mem::take(&mut self.offered) {
            v.push(Do::Restore);
        }
        v.push(Do::End(r));
        v
    }

    fn moved(&self) -> String {
        if self.kind == Kind::Enter { NO_KEY.into() } else { MOVED.into() }
    }

    /// Ends it from outside (Georg stopped Durchblick, the offer found no data device). `served`: the program has
    /// already been handed the paste, so the text is in and the stop says so.
    pub fn stop(&mut self, why: &str, served: bool) -> Vec<Do> {
        match self.phase {
            Phase::Over => Vec::new(),
            Phase::Paste if served => self.end(Ok(Route::Paste)),
            Phase::Settle(r) => self.end(Ok(r)),
            _ => self.end(Err(why.to_string())),
        }
    }

    /// The text is in the field (committed, or the paste handed over) or the key pressed.
    pub fn written(&self, served: bool) -> bool {
        matches!(self.phase, Phase::Settle(_)) || (self.phase == Phase::Paste && served)
    }

    pub fn step(&mut self, now: Instant, s: &Seen) -> Vec<Do> {
        let waited = now.saturating_duration_since(self.since);
        match self.phase {
            Phase::Over => Vec::new(),
            // The text is in: what Georg does now changes nothing about it.
            Phase::Settle(r) if s.hand || waited >= SETTLE => self.end(Ok(r)),
            Phase::Settle(_) => Vec::new(),
            // The program has the text already (its pipe was written at once): a hand or a focus change now is late.
            Phase::Paste if s.served => {
                (self.phase, self.since, self.offered) = (Phase::Settle(Route::Paste), now, false);
                vec![Do::Restore]
            }
            _ if s.hand => self.end(Err(HAND.into())),
            _ if !s.focus_here => {
                let m = self.moved();
                self.end(Err(m))
            }
            Phase::Input => self.input(now, waited, s),
            Phase::Select(route) => match s.selected {
                Some(true) => self.check(now, route, Replace::Empty),
                Some(false) => self.end(Err(NOT_EMPTIED.into())),
                None if waited >= CHECK_WAIT => self.end(Err(UNCHECKED.into())),
                None => Vec::new(),
            },
            Phase::Check(route, replace) => match (s.focused, route, s.field) {
                (Some(true), Route::Paste, _) => {
                    (self.phase, self.since, self.offered) = (Phase::Paste, now, true);
                    vec![Do::Offer, Do::PasteKey]
                }
                (Some(true), _, Field::Secret) => self.end(Err(SECRET.into())),
                // The text input went away while atspid was asked: the field it stood for is not known any more.
                (Some(true), Route::TextInput, Field::None) => {
                    let m = self.moved();
                    self.end(Err(m))
                }
                // The proof must still hold now: the program may have moved its keyboard while atspid was asked.
                (Some(true), Route::TextInput | Route::Enter, Field::Enabled) if !still(s.proof, replace) => {
                    let m = if s.proof == Proof::Elsewhere { ELSEWHERE.into() } else { self.moved() };
                    self.end(Err(m))
                }
                (Some(true), Route::TextInput, Field::Enabled) => self.settle(now, Route::TextInput, Do::Commit(replace)),
                (Some(true), Route::Enter, _) => self.settle(now, Route::Enter, Do::EnterKey),
                (Some(false), ..) => {
                    let m = self.moved();
                    self.end(Err(m))
                }
                (None, ..) if waited >= CHECK_WAIT => self.end(Err(UNCHECKED.into())),
                (None, ..) => Vec::new(),
            },
            Phase::Paste if waited >= PASTE_WAIT => self.end(Err(TIMEOUT.into())),
            Phase::Paste => Vec::new(),
        }
    }

    fn input(&mut self, now: Instant, waited: Duration, s: &Seen) -> Vec<Do> {
        if s.field == Field::Secret {
            return self.end(Err(SECRET.into()));
        }
        if s.field == Field::Enabled {
            self.last = s.proof;
            if let Proof::Here(replace) = s.proof {
                return match (self.kind, replace) {
                    (Kind::Enter, _) => self.check(now, Route::Enter, Replace::Empty),
                    (Kind::Text, Replace::Select) => self.select(now, Route::TextInput),
                    (Kind::Text, r) => self.check(now, Route::TextInput, r),
                };
            }
        }
        if waited < INPUT_WAIT {
            return Vec::new();
        }
        match (s.field, self.last) {
            (Field::Enabled, Proof::Elsewhere) => self.end(Err(ELSEWHERE.into())),
            (Field::Enabled, _) => self.end(Err(UNPROVEN.into())),
            // It spoke text-input there a moment ago and shows no field now: not proven, no paste or bare key.
            _ if self.had_input => self.end(Err(UNPROVEN.into())),
            // No text input: the bare key, or the paste after the field's text was selected.
            _ if self.kind == Kind::Enter => self.check(now, Route::Enter, Replace::Empty),
            _ => match replace_blind(self.value.as_deref()) {
                Replace::Select => self.select(now, Route::Paste),
                r => self.check(now, Route::Paste, r),
            },
        }
    }

    fn select(&mut self, now: Instant, then: Route) -> Vec<Do> {
        (self.phase, self.since) = (Phase::Select(then), now);
        vec![Do::Select]
    }

    fn check(&mut self, now: Instant, then: Route, r: Replace) -> Vec<Do> {
        (self.phase, self.since) = (Phase::Check(then, r), now);
        vec![Do::Check]
    }

    fn settle(&mut self, now: Instant, r: Route, d: Do) -> Vec<Do> {
        (self.phase, self.since) = (Phase::Settle(r), now);
        vec![d]
    }
}

/// Proof (ii) at the commit: still this field, and a deletion still the same bytes.
fn still(now: Proof, planned: Replace) -> bool {
    match (now, planned) {
        (Proof::Here(r), Replace::Delete(..)) => r == planned,
        (Proof::Here(_), _) => true,
        _ => false,
    }
}

/// B (EINGABE2): where a written text ended up, by the window read again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landed {
    /// The field holds exactly the text.
    Arrived,
    /// The field's text cannot be read.
    Unchecked,
    /// Not in the field; `Some(name)`: another field of the window changed by exactly that text.
    Missing(Option<String>),
}

/// Whether `after` is `before` with `text` put in somewhere (or `text` instead of it).
fn changed_by(before: &str, after: &str, text: &str) -> bool {
    after != before && (after == text || after.match_indices(text).any(|(i, _)| format!("{}{}", &after[..i], &after[i + text.len()..]) == before))
}

/// Whether `v` is `text` plus what the program put after it on its own and selected (Chromium's address bar
/// completes a typed text inline), by the text input's state after the write (`after`: text, cursor, anchor).
fn completed(v: &str, text: &str, after: Option<&(String, u32, u32)>) -> bool {
    let Some((t, c, a)) = after else { return false };
    let (lo, hi) = ((*c).min(*a) as usize, (*c).max(*a) as usize);
    v.len() > text.len() && v.starts_with(text) && t == v && lo == text.len() && hi == v.len()
}

/// `target`: the field's text now (None: unreadable); `others`: every other field as (name, before, after);
/// `after`: the program's text input state after the write, if it gave one.
pub fn landed(target: Option<&str>, text: &str, others: &[(String, Option<String>, Option<String>)], after: Option<&(String, u32, u32)>) -> Landed {
    match target {
        Some(v) if v == text || completed(v, text, after) => Landed::Arrived,
        None => Landed::Unchecked,
        Some(_) => Landed::Missing(
            others
                .iter()
                .find(|(_, b, a)| matches!((b, a), (Some(b), Some(a)) if changed_by(b, a, text)))
                .map(|(n, ..)| n.clone()),
        ),
    }
}

/// The German words for a text that did not arrive.
pub fn missing(name: &str, elsewhere: Option<&str>) -> String {
    let mut s = format!("Der Text ist nicht in „{}“ angekommen", name.trim());
    if let Some(o) = elsewhere {
        s.push_str(&format!("; er steht jetzt in „{}“", o.trim()));
    }
    s.push_str(". Der Bildschirm ist nicht wie geplant.");
    s
}

/// App ids of terminals: a paste there could run a command.
pub fn terminal(app: &str) -> bool {
    let a = app.to_lowercase();
    ["foot", "kitty", "alacritty", "konsole", "xterm", "wezterm", "terminal", "console", "ghostty", "terminator", "tilix", "urxvt"].iter().any(|t| a.contains(t))
}

#[cfg(test)]
#[path = "desk_write_tests.rs"]
mod tests;
