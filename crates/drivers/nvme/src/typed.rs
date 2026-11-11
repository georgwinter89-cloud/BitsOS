//! The proof that Georg typed the confirmation himself (NV-2p, pure: host-tested): the line is read back out of
//! the kernel's input sink, from the key presses of real devices, not from anything a program hands in. MCP's
//! `send_keys` and a program writing `/dev/input/event*` carry `injected` (R8c) and refuse the whole confirmation.
//! Stricter than `power_off_verdict`'s "the last Enter was physical": every key of the line is checked, against
//! the expected characters.

use alloc::vec::Vec;
use console::evdev::{EV_KEY, InputEvent};

const KEY_BACKSPACE: u16 = 14;
const KEY_ENTER: u16 = 28;
const KEY_KPENTER: u16 = 96;
/// Shift, Ctrl, Alt, Meta, the lock keys: they type nothing.
const MODIFIERS: [u16; 11] = [29, 42, 54, 56, 58, 69, 70, 97, 100, 125, 126];

/// What a confirmation may fail on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// A key of this line came from a program or MCP.
    Injected,
    /// No Enter since the prompt.
    NoEnter,
    /// The sink's look-back no longer reaches the prompt (much mouse moving): type again.
    TooOld,
    /// The Enter was pressed too long ago.
    Stale,
    /// The keys are not the expected characters.
    Wrong,
}

impl Refused {
    pub fn word(self) -> &'static str {
        match self {
            Refused::Injected => "injected",
            Refused::NoEnter => "no_enter",
            Refused::TooOld => "too_old",
            Refused::Stale => "stale",
            Refused::Wrong => "wrong_code",
        }
    }
}

/// The key codes that type `c` on an US or a German keyboard (Y and Z trade places; evdev reports positions).
/// None: a character that has no one place (a serial's `-`, `_`, `.`): any key that types something counts for it.
fn places(c: char) -> Option<[u16; 2]> {
    const LETTERS: [u16; 26] = [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44];
    match c.to_ascii_lowercase() {
        'y' | 'z' => Some([21, 44]),
        c @ 'a'..='z' => {
            let k = LETTERS[(c as u8 - b'a') as usize];
            Some([k, k])
        }
        '0' => Some([11, 82]),
        c @ '1'..='9' => {
            // Top row 2..=10; keypad 79 80 81 / 75 76 77 / 71 72 73.
            const PAD: [u16; 9] = [79, 80, 81, 75, 76, 77, 71, 72, 73];
            let d = (c as u8 - b'0') as usize;
            Some([d as u16 + 1, PAD[d - 1]])
        }
        _ => None,
    }
}

/// Whether the pressed `keys` type `code`.
pub fn types(keys: &[u16], code: &str) -> bool {
    let want: Vec<char> = code.chars().collect();
    keys.len() == want.len() && keys.iter().zip(&want).all(|(k, c)| places(*c).is_none_or(|p| p.contains(k)))
}

/// Judges the events since the prompt. `complete`: the look-back still starts at the prompt. Key repeats do not
/// type; Backspace takes the last key back. Ok: the line typed `code` on real devices, ended by an Enter at most
/// `max_age_ns` ago.
pub fn judge(events: &[InputEvent], complete: bool, code: &str, now_ns: u64, max_age_ns: u64) -> Result<(), Refused> {
    if !complete {
        return Err(Refused::TooOld);
    }
    let mut keys: Vec<u16> = Vec::new();
    let mut enter = None;
    for e in events.iter().filter(|e| e.kind == EV_KEY && e.value == 1) {
        if e.injected {
            return Err(Refused::Injected);
        }
        if enter.is_some() {
            // Keys after the Enter belong to nothing: the tool sends `confirm` right after reading its line.
            continue;
        }
        match e.code {
            KEY_ENTER | KEY_KPENTER => enter = Some(e.time_ns),
            KEY_BACKSPACE => {
                keys.pop();
            }
            c if MODIFIERS.contains(&c) => {}
            c => keys.push(c),
        }
    }
    let Some(t) = enter else { return Err(Refused::NoEnter) };
    if now_ns.saturating_sub(t) > max_age_ns {
        return Err(Refused::Stale);
    }
    if !types(&keys, code) {
        return Err(Refused::Wrong);
    }
    Ok(())
}
