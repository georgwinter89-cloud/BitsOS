//! DB3, EINS-B: the popup's own settings (owner bitscomp): the key that opens it on the keyboard and on the
//! gamepad, and the assistant's view keys (`assist_level`). atspid's switch `desktop.durchblick.programs` is shown
//! with them (`group_key`). The old Durchblick keys (who answers, previews, per-program Automodus, the question
//! before what cannot be undone) went with the one level `ai.level` (PLAN K8): `GONE` lists them for settingsd.

use mcpreg::Json;
use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::settings::Values;

pub const TASTE: &str = "durchblick.taste";
pub const PAD: &str = "durchblick.pad";
pub const PROGRAMS: &str = "desktop.durchblick.programs";
/// The settings page's group (and the address `durchblick`).
pub const GROUP: &str = "durchblick";
/// PLAN K8: the old keys bitscomp registered, unregistered (their values dropped) once the engine's `ai.level` is
/// there (its migration reads them first, eins-b.md).
pub const GONE: [&str; 4] = ["durchblick.antwort", "durchblick.vorschau", "durchblick.bild", "durchblick.handeln"];

pub const TASTEN: [&str; 4] = ["win_f1", "win_h", "strg_alt_h", "aus"];
const TASTEN_DE: [&str; 4] = ["Win+F1", "Win+H", "Strg+Alt+H", "Aus"];
pub const PADS: [&str; 3] = ["select_y", "select_x", "aus"];
const PADS_DE: [&str; 3] = ["Select und Y", "Select und X", "Aus"];

/// The keys in the page's order (`page_schema::HAND`).
pub const ORDER: [(&str, &str); 3] = [(TASTE, "Tastenkürzel"), (PAD, "Gamepad"), (PROGRAMS, "Fremde Programme lesen")];

/// The settings page's group of `key`: atspid's switch belongs to Durchblick.
pub fn group_key(key: &str) -> &str {
    if key == PROGRAMS { "durchblick" } else { key }
}

fn def(key: &str, schema: Json, default: Json, de: &str, en: &str) -> Json {
    Json::obj(vec![
        ("key", Json::str(key)),
        ("schema", schema),
        ("default", default),
        ("text", Json::obj(vec![("de", Json::str(de)), ("en", Json::str(en))])),
        ("restart", Json::str("none")),
        ("safety", Json::str("normal")),
    ])
}

fn choice(values: &[&str], de: &[&str]) -> Json {
    let arr = |v: &[&str]| Json::Arr(v.iter().map(|s| Json::str(s)).collect());
    Json::obj(vec![("type", Json::str("choice")), ("choices", arr(values)), ("choices_de", arr(de))])
}

/// bitscomp's keys of the popup and the assistant's views.
pub fn defs() -> Vec<Json> {
    let mut v = vec![
        def(TASTE, choice(&TASTEN, &TASTEN_DE), Json::str("win_f1"), "Taste für den Assistenten als Popup", "Key for the assistant's popup"),
        def(PAD, choice(&PADS, &PADS_DE), Json::str("select_y"), "Gamepad-Tasten für den Assistenten als Popup", "Gamepad buttons for the assistant's popup"),
    ];
    v.extend(crate::assist_level::defs());
    v
}

/// The key combination that opens the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taste {
    WinF1,
    WinH,
    StrgAltH,
    Aus,
}

impl Taste {
    pub fn of(v: &Values) -> Taste {
        match v.text(TASTE).unwrap_or("win_f1") {
            "win_h" => Taste::WinH,
            "strg_alt_h" => Taste::StrgAltH,
            "aus" => Taste::Aus,
            _ => Taste::WinF1,
        }
    }

    pub fn hits(self, sym: Keysym, m: &ModifiersState) -> bool {
        match self {
            Taste::WinF1 => m.logo && !m.ctrl && !m.alt && sym == Keysym::F1,
            Taste::WinH => m.logo && !m.ctrl && !m.alt && matches!(sym, Keysym::h | Keysym::H),
            Taste::StrgAltH => m.ctrl && m.alt && !m.logo && matches!(sym, Keysym::h | Keysym::H),
            Taste::Aus => false,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Taste::WinF1 => "Win+F1",
            Taste::WinH => "Win+H",
            Taste::StrgAltH => "Strg+Alt+H",
            Taste::Aus => "",
        }
    }
}

/// Which button with Select opens the panel on the gamepad (None: off).
pub fn pad_button(v: &Values) -> Option<crate::pad::Btn> {
    match v.text(PAD).unwrap_or("select_y") {
        "select_x" => Some(crate::pad::Btn::X),
        "aus" => None,
        _ => Some(crate::pad::Btn::Y),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_their_defaults() {
        let v = Values::default();
        assert_eq!((Taste::of(&v), pad_button(&v)), (Taste::WinF1, Some(crate::pad::Btn::Y)));
        let none = ModifiersState::default();
        let win = ModifiersState { logo: true, ..none };
        assert!(Taste::WinF1.hits(Keysym::F1, &win) && !Taste::WinF1.hits(Keysym::F1, &none) && !Taste::WinF1.hits(Keysym::F2, &win));
        let mut v = Values::default();
        v.set(TASTE, "strg_alt_h");
        v.set(PAD, "aus");
        let ca = ModifiersState { ctrl: true, alt: true, ..none };
        assert!(Taste::of(&v).hits(Keysym::h, &ca) && !Taste::of(&v).hits(Keysym::F1, &win));
        assert_eq!(pad_button(&v), None);
    }

    #[test]
    fn every_key_is_a_valid_definition_and_none_of_the_old_ones() {
        let keys: Vec<String> = defs().iter().map(|d| d.get("key").and_then(Json::as_str).unwrap().to_string()).collect();
        assert_eq!(keys, [TASTE, PAD, crate::assist_level::SIDEBAR_SEES, crate::assist_level::NEVER_SEE]);
        for d in defs() {
            assert!(d.get("text").and_then(|t| t.get("de")).and_then(Json::as_str).is_some_and(|t| t.len() > 8));
            assert_eq!(d.get("safety").and_then(Json::as_str), Some("normal"));
        }
        assert!(keys.iter().all(|k| !GONE.iter().any(|g| k.starts_with(g))), "no removed key is registered again");
        assert_eq!(group_key(PROGRAMS), "durchblick");
    }
}
