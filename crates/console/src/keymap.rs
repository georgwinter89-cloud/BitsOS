//! The German (QWERTZ) keymap of the console: Linux key codes -> characters and special keys
//! (V1 `BitsOS-ASM/keyboard.asm:658-678` `altgr_map`/`keypad`/`keymap`/`keymap_shift`, extended with the AltGr
//! keys of a German keyboard: ² ³ € µ and `<>|` on the ISO key). Dead keys are off: `^`, `´` and `` ` `` type at once.
//!
//! Rules (in this order, as V1 `kbd_byte`):
//! 1. **AltGr before Ctrl** (`keyboard.asm:372`): Windows, and QEMU/VNC from a Windows host, send AltGr as
//!    Ctrl+Alt. Right Alt, or Ctrl together with left Alt, on a key with an AltGr character types that
//!    character; Ctrl is not looked at for it.
//! 2. Ctrl + a letter gives the control character (Ctrl+C = 0x03, Ctrl+D = 0x04, by the letter printed on the
//!    German key: Ctrl+Z is the key right of T).
//! 3. Shift and Caps Lock: Caps Lock shifts letters only (also ä ö ü), Shift with Caps Lock gives lower case.
//! 4. Keypad: Num Lock on gives digits and `,`, off gives the navigation keys; `/ * - +` and Enter always.

/// Linux key codes used by name here (include/uapi/linux/input-event-codes.h).
pub mod code {
    pub const ESC: u16 = 1;
    pub const BACKSPACE: u16 = 14;
    pub const TAB: u16 = 15;
    pub const ENTER: u16 = 28;
    pub const LEFTCTRL: u16 = 29;
    pub const LEFTSHIFT: u16 = 42;
    pub const RIGHTSHIFT: u16 = 54;
    pub const KPASTERISK: u16 = 55;
    pub const LEFTALT: u16 = 56;
    pub const SPACE: u16 = 57;
    pub const CAPSLOCK: u16 = 58;
    pub const F1: u16 = 59;
    pub const F10: u16 = 68;
    pub const NUMLOCK: u16 = 69;
    pub const SCROLLLOCK: u16 = 70;
    pub const KP7: u16 = 71;
    pub const KPMINUS: u16 = 74;
    pub const KPPLUS: u16 = 78;
    pub const KPDOT: u16 = 83;
    pub const KEY_102ND: u16 = 86;
    pub const F11: u16 = 87;
    pub const F12: u16 = 88;
    pub const KPENTER: u16 = 96;
    pub const RIGHTCTRL: u16 = 97;
    pub const KPSLASH: u16 = 98;
    pub const RIGHTALT: u16 = 100;
    pub const HOME: u16 = 102;
    pub const UP: u16 = 103;
    pub const PAGEUP: u16 = 104;
    pub const LEFT: u16 = 105;
    pub const RIGHT: u16 = 106;
    pub const END: u16 = 107;
    pub const DOWN: u16 = 108;
    pub const PAGEDOWN: u16 = 109;
    pub const INSERT: u16 = 110;
    pub const DELETE: u16 = 111;
    pub const LEFTMETA: u16 = 125;
    pub const RIGHTMETA: u16 = 126;
    pub const D: u16 = 32;
    pub const J: u16 = 36;
}
use code::*;

/// Modifier and lock state at the moment of a key press.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    /// Left Alt.
    pub alt: bool,
    /// Right Alt.
    pub altgr: bool,
    pub caps: bool,
    pub num: bool,
}

/// What a key press means to the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// A control character (Ctrl+letter, 1..=26).
    Ctrl(u8),
    Enter,
    Backspace,
    Tab,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    /// Nothing to type (a modifier, a lock key, an unmapped combination).
    None,
}

/// Key code -> (plain, shift, AltGr) on the German layout; '\0' = none. Codes 2..=57 and the ISO key.
const DE: &[(u16, [char; 3])] = &[
    (2, ['1', '!', '\0']),
    (3, ['2', '"', '²']),
    (4, ['3', '§', '³']),
    (5, ['4', '$', '\0']),
    (6, ['5', '%', '\0']),
    (7, ['6', '&', '\0']),
    (8, ['7', '/', '{']),
    (9, ['8', '(', '[']),
    (10, ['9', ')', ']']),
    (11, ['0', '=', '}']),
    (12, ['ß', '?', '\\']),
    (13, ['´', '`', '\0']),
    (16, ['q', 'Q', '@']),
    (17, ['w', 'W', '\0']),
    (18, ['e', 'E', '€']),
    (19, ['r', 'R', '\0']),
    (20, ['t', 'T', '\0']),
    (21, ['z', 'Z', '\0']),
    (22, ['u', 'U', '\0']),
    (23, ['i', 'I', '\0']),
    (24, ['o', 'O', '\0']),
    (25, ['p', 'P', '\0']),
    (26, ['ü', 'Ü', '\0']),
    (27, ['+', '*', '~']),
    (30, ['a', 'A', '\0']),
    (31, ['s', 'S', '\0']),
    (32, ['d', 'D', '\0']),
    (33, ['f', 'F', '\0']),
    (34, ['g', 'G', '\0']),
    (35, ['h', 'H', '\0']),
    (36, ['j', 'J', '\0']),
    (37, ['k', 'K', '\0']),
    (38, ['l', 'L', '\0']),
    (39, ['ö', 'Ö', '\0']),
    (40, ['ä', 'Ä', '\0']),
    (41, ['^', '°', '\0']),
    (43, ['#', '\'', '\0']),
    (44, ['y', 'Y', '\0']),
    (45, ['x', 'X', '\0']),
    (46, ['c', 'C', '\0']),
    (47, ['v', 'V', '\0']),
    (48, ['b', 'B', '\0']),
    (49, ['n', 'N', '\0']),
    (50, ['m', 'M', 'µ']),
    (51, [',', ';', '\0']),
    (52, ['.', ':', '\0']),
    (53, ['-', '_', '\0']),
    (57, [' ', ' ', '\0']),
    (KEY_102ND, ['<', '>', '|']),
];

fn row(code: u16) -> Option<&'static [char; 3]> {
    DE.iter().find(|(c, _)| *c == code).map(|(_, r)| r)
}

/// True for modifier and lock keys (they never type and never repeat).
pub fn is_modifier(code: u16) -> bool {
    matches!(code, LEFTCTRL | RIGHTCTRL | LEFTSHIFT | RIGHTSHIFT | LEFTALT | RIGHTALT | LEFTMETA | RIGHTMETA | CAPSLOCK | NUMLOCK | SCROLLLOCK)
}

/// The key code -> meaning under `m`.
pub fn translate(code: u16, m: &Mods) -> Key {
    match code {
        ESC => return Key::Esc,
        BACKSPACE => return Key::Backspace,
        TAB => return Key::Tab,
        ENTER | KPENTER => return Key::Enter,
        HOME => return Key::Home,
        UP => return Key::Up,
        PAGEUP => return Key::PageUp,
        LEFT => return Key::Left,
        RIGHT => return Key::Right,
        END => return Key::End,
        DOWN => return Key::Down,
        PAGEDOWN => return Key::PageDown,
        INSERT => return Key::Insert,
        DELETE => return Key::Delete,
        F1..=F10 => return Key::F((code - F1 + 1) as u8),
        F11 => return Key::F(11),
        F12 => return Key::F(12),
        KPSLASH => return Key::Char('/'),
        KPASTERISK => return Key::Char('*'),
        KPMINUS => return Key::Char('-'),
        KPPLUS => return Key::Char('+'),
        KP7..=KPDOT => return keypad(code, m.num),
        _ => {}
    }
    if is_modifier(code) {
        return Key::None;
    }
    let Some(r) = row(code) else { return Key::None };
    // 1. AltGr before Ctrl.
    if m.altgr || (m.ctrl && m.alt) {
        return if r[2] != '\0' { Key::Char(r[2]) } else { Key::None };
    }
    // 2. Ctrl + letter.
    if m.ctrl {
        return match r[0] {
            c @ 'a'..='z' => Key::Ctrl(c as u8 - b'a' + 1),
            _ => Key::None,
        };
    }
    if m.alt {
        return Key::None;
    }
    // 3. Shift and Caps Lock.
    // A letter has an upper-case Shift level; ß (Shift: ?) is lower case but no Caps Lock letter.
    let letter = r[0].is_lowercase() && r[1].is_uppercase();
    let shifted = m.shift ^ (m.caps && letter);
    let c = if shifted { r[1] } else { r[0] };
    if c == '\0' { Key::None } else { Key::Char(c) }
}

fn keypad(code: u16, num: bool) -> Key {
    // KP7 KP8 KP9 KPMINUS KP4 KP5 KP6 KPPLUS KP1 KP2 KP3 KP0 KPDOT (codes 71..=83).
    const DIGITS: &[u8; 13] = b"789-456+1230,";
    const NAV: [Key; 13] = [
        Key::Home, Key::Up, Key::PageUp, Key::Char('-'), Key::Left, Key::None, Key::Right, Key::Char('+'), Key::End,
        Key::Down, Key::PageDown, Key::Insert, Key::Delete,
    ];
    let i = (code - KP7) as usize;
    if num { Key::Char(DIGITS[i] as char) } else { NAV[i] }
}

/// Modifier bits for `reverse`.
pub const SHIFT: u8 = 1;
pub const ALTGR: u8 = 2;

/// Which key (and Shift/AltGr) types `c` on this keymap (MC's `send_keys` can use it instead of its own table).
pub fn reverse(c: char) -> Option<(u16, u8)> {
    match c {
        '\n' | '\r' => return Some((ENTER, 0)),
        '\t' => return Some((TAB, 0)),
        '\u{8}' => return Some((BACKSPACE, 0)),
        '\u{1b}' => return Some((ESC, 0)),
        '\0' => return None,
        _ => {}
    }
    for &(code, r) in DE {
        for (i, m) in [(0usize, 0u8), (1, SHIFT), (2, ALTGR)] {
            if r[i] == c {
                return Some((code, m));
            }
        }
    }
    None
}

/// Bytes a special key sends to a program in raw mode (VT100 / Linux console sequences); None for none.
pub fn raw_sequence(k: Key) -> Option<&'static str> {
    Some(match k {
        Key::Up => "\x1b[A",
        Key::Down => "\x1b[B",
        Key::Right => "\x1b[C",
        Key::Left => "\x1b[D",
        Key::Home => "\x1b[1~",
        Key::Insert => "\x1b[2~",
        Key::Delete => "\x1b[3~",
        Key::End => "\x1b[4~",
        Key::PageUp => "\x1b[5~",
        Key::PageDown => "\x1b[6~",
        // The Linux console's function keys (the default keymap's `F1`..`F12` strings).
        Key::F(1) => "\x1b[[A",
        Key::F(2) => "\x1b[[B",
        Key::F(3) => "\x1b[[C",
        Key::F(4) => "\x1b[[D",
        Key::F(5) => "\x1b[[E",
        Key::F(6) => "\x1b[17~",
        Key::F(7) => "\x1b[18~",
        Key::F(8) => "\x1b[19~",
        Key::F(9) => "\x1b[20~",
        Key::F(10) => "\x1b[21~",
        Key::F(11) => "\x1b[23~",
        Key::F(12) => "\x1b[24~",
        _ => return None,
    })
}

/// The bytes a Linux VT sends to the terminal for a key (IN-4): characters as UTF-8 (K_UNICODE), Enter `\r`,
/// Backspace DEL (0x7f), Tab, Esc, Ctrl+key the control byte, the other keys their console sequences. The line
/// discipline turns them into lines, signals and echo by the termios.
pub fn key_bytes(k: Key, b: &mut [u8; 8]) -> &[u8] {
    let n = match k {
        Key::Char(c) => c.encode_utf8(&mut b[..]).len(),
        Key::Ctrl(n) => {
            b[0] = n;
            1
        }
        Key::Enter => {
            b[0] = b'\r';
            1
        }
        Key::Backspace => {
            b[0] = 0x7F;
            1
        }
        Key::Tab => {
            b[0] = b'\t';
            1
        }
        Key::Esc => {
            b[0] = 0x1B;
            1
        }
        k => match raw_sequence(k) {
            Some(seq) => {
                let s = seq.as_bytes();
                b[..s.len()].copy_from_slice(s);
                s.len()
            }
            None => 0,
        },
    };
    &b[..n]
}
