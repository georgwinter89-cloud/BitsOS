//! DB1: text as key presses on the German layout bitscomp's keyboard uses (`XkbConfig { layout: "de" }`), so
//! `ui-act ... type` goes the same way a typed character does (evdev codes, then xkb).

pub const SHIFT: u32 = 42;
/// AltGr: ISO_Level3_Shift on the right Alt key in the German layout.
pub const ALTGR: u32 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mod {
    None,
    Shift,
    AltGr,
}

/// The key (evdev code) and the modifier that give `c`; None for a character the layout has no plain key for.
pub fn key_of(c: char) -> Option<(u32, Mod)> {
    use Mod::{AltGr, None as N, Shift};
    const ROW1: &str = "1234567890ß";
    const ROW1_SHIFT: &str = "!\"§$%&/()=?";
    const Q: &str = "qwertzuiopü+";
    const A: &str = "asdfghjklöä#";
    const Y: &str = "yxcvbnm,.-";
    const Y_SHIFT: &str = "YXCVBNM;:_";
    let pos = |s: &str, c: char| s.chars().position(|x| x == c).map(|p| p as u32);
    let a_code = |p: u32| if p < 11 { 30 + p } else { 43 };
    Some(match c {
        ' ' => (57, N),
        '\n' => (28, N),
        '\t' => (15, N),
        '<' => (86, N),
        '>' => (86, Shift),
        '|' => (86, AltGr),
        '*' => (27, Shift),
        '\'' => (43, Shift),
        '@' => (16, AltGr),
        '€' => (18, AltGr),
        '~' => (27, AltGr),
        '{' => (8, AltGr),
        '[' => (9, AltGr),
        ']' => (10, AltGr),
        '}' => (11, AltGr),
        '\\' => (12, AltGr),
        'µ' => (50, AltGr),
        '²' => (3, AltGr),
        '³' => (4, AltGr),
        '°' => (41, Shift),
        c if pos(ROW1, c).is_some() => (2 + pos(ROW1, c)?, N),
        c if pos(ROW1_SHIFT, c).is_some() => (2 + pos(ROW1_SHIFT, c)?, Shift),
        c if pos(Q, c).is_some() => (16 + pos(Q, c)?, N),
        c if pos(A, c).is_some() => (a_code(pos(A, c)?), N),
        c if pos(Y, c).is_some() => (44 + pos(Y, c)?, N),
        c if pos(Y_SHIFT, c).is_some() => (44 + pos(Y_SHIFT, c)?, Shift),
        c if c.is_uppercase() => {
            let lower = c.to_lowercase().next()?;
            if !(lower.is_ascii_lowercase() || "äöü".contains(lower)) {
                return None;
            }
            let (code, m) = key_of(lower)?;
            if m != N {
                return None;
            }
            (code, Shift)
        }
        _ => return None,
    })
}

/// The first character of `text` no key gives.
pub fn untypable(text: &str) -> Option<char> {
    text.chars().find(|c| key_of(*c).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn german_layout_keys() {
        assert_eq!(key_of('a'), Some((30, Mod::None)));
        assert_eq!(key_of('A'), Some((30, Mod::Shift)));
        assert_eq!(key_of('z'), Some((21, Mod::None)), "German: z where the US layout has y");
        assert_eq!(key_of('y'), Some((44, Mod::None)));
        assert_eq!(key_of('ä'), Some((40, Mod::None)));
        assert_eq!(key_of('Ü'), Some((26, Mod::Shift)));
        assert_eq!(key_of('#'), Some((43, Mod::None)));
        assert_eq!(key_of('ß'), Some((12, Mod::None)));
        assert_eq!(key_of('?'), Some((12, Mod::Shift)));
        assert_eq!(key_of('@'), Some((16, Mod::AltGr)));
        assert_eq!(key_of('/'), Some((8, Mod::Shift)));
        assert_eq!(key_of('-'), Some((53, Mod::None)));
        assert_eq!(key_of('_'), Some((53, Mod::Shift)));
        assert_eq!(key_of('0'), Some((11, Mod::None)));
        assert_eq!(key_of('='), Some((11, Mod::Shift)));
        assert_eq!(key_of(':'), Some((52, Mod::Shift)));
        assert_eq!(untypable("einstellungen/aussehen 12"), None);
        assert_eq!(untypable("a😀"), Some('😀'));
        assert_eq!(key_of('ẞ'), None, "no plain key for the capital sharp s");
    }
}
