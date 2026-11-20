//! VB-1: the question "Version N läuft. Bestätigen?" on the report page (the Gaming-PC's safe mode has no desktop
//! to ask on). It stands at the foot of every page while the kernel asks, answered by one key of a real keyboard:
//! J is "Ja", Z "Zurück zur vorherigen" (the German layout's Z, evdev KEY_Y), any other key turns the page as
//! before. The kernel's report thread words the state through `lines`; the console takes the keys.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// evdev key codes (linux-7.3.1/include/uapi/linux/input-event-codes.h): J, and the key the German layout
/// labels Z (KEY_Y, 21).
pub const KEY_JA: u16 = 36;
pub const KEY_ZURUECK: u16 = 21;

/// Where "Zurück zur vorherigen" goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Back {
    /// The newest confirmed version below the running one.
    To(u32),
    /// There is no older confirmed version: only "Ja".
    Nothing,
    /// The medium could not be read yet (an install holds it): only "Ja" for now, the kernel looks again.
    Unread,
}

/// Where the question stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Asked: "Ja", and "Zurück" when an older confirmed version is there. `again`: a "Ja" before could not be
    /// written; `early`: a J or Z came before the key counted and was not taken.
    Asking { back: Back, again: bool, early: bool },
    /// "Ja" given: the kernel writes OK.
    Confirming,
    /// OK is written (`by`: Georg's key, or a proof that came first).
    Confirmed { by_georg: bool },
    /// "Zurück" given: the pin is set, the restart comes.
    Back { to: u32 },
    /// Nothing changed (why, plain German).
    Failed(String),
}

impl State {
    /// A plain question: no note, `back` as read.
    pub fn asking(back: Back) -> State {
        State::Asking { back, again: false, early: false }
    }
}

/// The foot's rows for version `version` (no rows: no question this boot).
pub fn lines(version: u32, s: &State) -> Vec<String> {
    let one = |t: String| alloc::vec![t];
    match s {
        State::Asking { back, again, early } => {
            let mut v = Vec::new();
            if *again {
                v.push(String::from("Die Bestätigung konnte nicht geschrieben werden; es ist nichts geändert."));
            }
            v.push(format!("Version {version} läuft. Bestätigen?"));
            v.push(match back {
                Back::To(b) => format!("Taste J: Ja (Version {version} bleibt).   Taste Z: Zurück zur vorherigen (Version {b}, BitsOS startet dann neu)."),
                Back::Nothing => format!("Taste J: Ja (Version {version} bleibt). Eine ältere bestätigte Version gibt es nicht."),
                Back::Unread => format!("Taste J: Ja (Version {version} bleibt)."),
            });
            if *early {
                v.push(String::from("Die Taste kam zu früh und zählt nicht: bitte in zwei Sekunden noch einmal drücken."));
            }
            v
        }
        State::Confirming => one(format!("Version {version} wird bestätigt …")),
        State::Confirmed { by_georg: true } => one(format!("Version {version} ist bestätigt. Sie startet ab jetzt immer.")),
        State::Confirmed { by_georg: false } => one(format!("Version {version} ist bestätigt (ohne Frage). Sie startet ab jetzt immer.")),
        State::Back { to } => one(format!("Zurück zu Version {to}: BitsOS startet in wenigen Sekunden neu.")),
        State::Failed(why) => one(format!("{why} Es bleibt bei Version {version}; ohne Bestätigung startet beim nächsten Mal die vorherige.")),
    }
}

/// What a real key means while the question stands (pure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Ja,
    Zurueck,
    /// J or Z before the key counts: taken (the page neither turns nor ends) and nothing answered.
    Early,
    /// Any other key: it turns the page as before.
    Page,
}

/// What a real key does (pure): only once armed, and Zurück only when there is somewhere to go back to.
pub fn key(s: &State, code: u16, armed: bool) -> Key {
    let State::Asking { back, .. } = s else { return Key::Page };
    let zurueck = matches!(back, Back::To(_));
    match code {
        KEY_JA if !armed => Key::Early,
        KEY_ZURUECK if zurueck && !armed => Key::Early,
        KEY_JA => Key::Ja,
        KEY_ZURUECK if zurueck => Key::Zurueck,
        _ => Key::Page,
    }
}

/// A start question on the text console after the page (the measurement run, `gpu-messung`): J starts, any other
/// key of a real keyboard ends it with nothing done. A key before the question counts (the Enter of the command's
/// own line, a key held over) is taken and answers nothing, so only a key pressed for the question decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Start {
    Ja,
    Nein,
    Early,
}

/// What a real key does to the start question (pure).
pub fn start_key(code: u16, armed: bool) -> Start {
    match (armed, code) {
        (false, _) => Start::Early,
        (true, KEY_JA) => Start::Ja,
        (true, _) => Start::Nein,
    }
}
