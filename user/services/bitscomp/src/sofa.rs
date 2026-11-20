//! GM-6a: the sofa mode (plan/spec/sofa-mode.md), the state and the pure rules: the mode switch of §5 as a state
//! machine (`decide`: every way in and out, Home over a running program, a pad plugged in), the scale k with the
//! overscan inset (§1), the settings keys of §6 (registered by bitscomp at run time, `defs`) and their values.
//! The sofa mode is a bitscomp layer above the workspaces and below the System layer (paint.rs); its programs run
//! on the workspace `Sofa`; no switch ever closes, pauses or restarts a program (sofa_app.rs does the switching).
//! Host-tested in `sofa_tests.rs`.

use std::time::Duration;

use mcpreg::Json;

use crate::layout::Rect;
use crate::settings::Values;
use crate::sofa_focus::Nav;

/// The workspace the sofa mode's programs run on (made on the first start, kept in the list).
pub const WORKSPACE: &str = "Sofa";
/// A pad plugged in before settingsd's values came waits this long for them (then the defaults decide).
pub const BOOT_WAIT: Duration = Duration::from_secs(3);

/// The search field's letters at most (one input's bound, R1; a held key's repeat cannot grow it further).
pub const TYPED_MAX: usize = 256;

/// The ways in and out (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    /// Home on a pad, or Select+Start held (PK-L1c).
    Home,
    WinEsc,
    /// The library's built-in tile "Sofa-Modus".
    Tile,
    /// The address `sofa/`.
    Address,
    /// A pad plugged in while bitscomp runs.
    PadPlug,
    /// A pad that was there when the desktop started.
    Boot,
    CtlOn,
    CtlOff,
    /// "Zum normalen Desktop" (quick menu, System section; GM-6c).
    Desktop,
    /// A workspace key or command (Ctrl+Alt+digit, new, remove): the desktop shows that workspace.
    Workspace,
}

impl Way {
    pub fn word(self) -> &'static str {
        match self {
            Way::Home => "Home",
            Way::WinEsc => "Win+Esc",
            Way::Tile => "library tile",
            Way::Address => "address sofa/",
            Way::PadPlug => "pad plugged in",
            Way::Boot => "pad at start",
            Way::CtlOn => "ctl sofa on",
            Way::CtlOff => "ctl sofa off",
            Way::Desktop => "Zum normalen Desktop",
            Way::Workspace => "a workspace key",
        }
    }
}

/// What a way does in the current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Enter,
    Leave,
    QuickOpen,
    QuickClose,
    Stay,
}

/// What the switch looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctx {
    pub active: bool,
    /// A program is in front on the `Sofa` workspace (the shell is behind it).
    pub program: bool,
    pub quick: bool,
    pub start_on_pad: bool,
}

/// §5: Home, Win+Esc, the tile, `sofa/`, `ctl sofa on` enter; a pad plugged in (or there at the start) enters while
/// `sofa.start_on_pad` is on. In the mode: Home closes an open quick menu, opens it over a program in front and
/// leaves from a sofa screen; Win+Esc, `ctl sofa off`, "Zum normalen Desktop" and a workspace key always leave. Nothing else
/// changes anything (a second way in stays, a way out outside the mode stays).
pub fn decide(c: Ctx, w: Way) -> Step {
    if !c.active {
        return match w {
            Way::Home | Way::WinEsc | Way::Tile | Way::Address | Way::CtlOn => Step::Enter,
            Way::PadPlug | Way::Boot if c.start_on_pad => Step::Enter,
            _ => Step::Stay,
        };
    }
    match w {
        Way::Home if c.quick => Step::QuickClose,
        Way::Home if c.program => Step::QuickOpen,
        Way::Home | Way::WinEsc | Way::CtlOff | Way::Desktop | Way::Workspace => Step::Leave,
        _ => Step::Stay,
    }
}

/// What is in front while the mode is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Front {
    /// The sofa screens.
    #[default]
    Shell,
    /// A program's window on `Sofa`, fullscreen: the pad is the program's except Home.
    Program,
}

/// What the sofa layer draws this frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    None,
    /// The shell alone, opaque (nothing under it is drawn).
    Shell,
    /// The quick menu over the program in front (the program keeps being drawn under it).
    Over,
}

/// The values of §6 (defaults where a key is missing or out of its range).
#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    pub start_on_pad: bool,
    pub motion: bool,
    pub sort: String,
    pub start_volume: i64,
    pub overscan: i64,
    /// `sofa.home_chord`: Select+Start held is Home on pads without one.
    pub chord: bool,
    pub pad_map: String,
}

impl Default for Prefs {
    fn default() -> Prefs {
        Prefs::of(&Values::default())
    }
}

impl Prefs {
    pub fn of(v: &Values) -> Prefs {
        let int = |k: &str, d: i64, lo: i64, hi: i64| v.int(k).filter(|n| (lo..=hi).contains(n)).unwrap_or(d);
        let choice = |k: &str, d: &str, of: &[&str]| v.text(k).filter(|s| of.contains(s)).unwrap_or(d).to_string();
        Prefs {
            start_on_pad: v.flag("sofa.start_on_pad").unwrap_or(true),
            // theme.motion off turns the lift off too (§1).
            motion: v.flag("sofa.motion").unwrap_or(true) && v.text("theme.motion") != Some("off"),
            sort: choice("sofa.sort", "zuletzt", &SORTS),
            start_volume: int("sofa.start_volume", 20, 0, 100),
            overscan: int("sofa.overscan", 100, 90, 100),
            chord: choice("sofa.home_chord", "select_start", &CHORDS) == "select_start",
            pad_map: v.text("sofa.pad.map").unwrap_or("").to_string(),
        }
    }
}

const SORTS: [&str; 3] = ["zuletzt", "az", "groesse"];
const CHORDS: [&str; 2] = ["select_start", "aus"];

/// The keys of §6, in its order.
pub const KEYS: [&str; 7] = ["sofa.start_on_pad", "sofa.motion", "sofa.sort", "sofa.start_volume", "sofa.overscan", "sofa.home_chord", "sofa.pad.map"];

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

fn int(min: i64, max: i64) -> Json {
    Json::obj(vec![("type", Json::str("int")), ("min", Json::Num(min as f64)), ("max", Json::Num(max as f64)), ("unit", Json::str("%"))])
}

/// The settingsd definitions of the keys of §6 (owner bitscomp, as the Absicht keys, settings_write.rs).
pub fn defs() -> Vec<Json> {
    let bool = || Json::obj(vec![("type", Json::str("bool"))]);
    vec![
        def("sofa.start_on_pad", bool(), Json::Bool(true), "Im Sofa-Modus starten, sobald ein Gamepad eingeschaltet wird", "Start the sofa mode when a gamepad is switched on"),
        def("sofa.motion", bool(), Json::Bool(true), "Kacheln heben sich, wenn sie gewählt sind", "Tiles lift when they are chosen"),
        def("sofa.sort", choice(&SORTS, &["Zuletzt gespielt", "A–Z", "Größe"]), Json::str("zuletzt"), "Sortierung der Bibliothek", "Library sort order"),
        def("sofa.start_volume", int(0, 100), Json::Num(20.0), "Lautstärke beim Start (nur wenn sie höher ist)", "Volume at start (only when it is higher)"),
        def("sofa.overscan", int(90, 100), Json::Num(100.0), "Bildausschnitt für Fernseher, die den Rand abschneiden", "Picture area for TVs that cut the edge"),
        def("sofa.home_chord", choice(&CHORDS, &["Select und Start", "Aus"]), Json::str("select_start"), "Home-Taste für Gamepads ohne Home: Select und Start zusammen", "Home for gamepads without one: Select and Start together"),
        def("sofa.pad.map", Json::obj(vec![("type", Json::str("text")), ("max_bytes", Json::Num(4096.0))]), Json::str(""), "Gelernte Tastenbelegung (vendor:product=a:b2,b:b1,...)", "Learned button mapping (vendor:product=a:b2,b:b1,...)"),
    ]
}

/// The sofa layout's place on the output (physical px): the output shrunk by `sofa.overscan` around its centre,
/// and k = min(w / 1920, h / 1080) of that (§1; theme.scale does not apply).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub k: f64,
}

impl Frame {
    pub fn of(out_w: i32, out_h: i32, overscan: i64) -> Frame {
        let o = overscan.clamp(90, 100) as f64 / 100.0;
        let (w, h) = ((out_w as f64 * o).round() as i32, (out_h as f64 * o).round() as i32);
        let k = (w as f64 / 1920.0).min(h as f64 / 1080.0).max(0.01);
        Frame { x: (out_w - w) / 2, y: (out_h - h) / 2, w: w.max(1), h: h.max(1), k }
    }

    /// The board size this frame shows (1920x1080 at 16:9; the longer side grows otherwise).
    pub fn board(&self) -> (i32, i32) {
        ((self.w as f64 / self.k).round() as i32, (self.h as f64 / self.k).round() as i32)
    }

    /// A board rect in physical px of the output.
    pub fn r(&self, b: Rect) -> Rect {
        let p = |v: i32| (v as f64 * self.k).round() as i32;
        let (x, y) = (self.x + p(b.x), self.y + p(b.y));
        Rect { x, y, w: self.x + p(b.x + b.w) - x, h: self.y + p(b.y + b.h) - y }
    }

    /// A physical point of the output as a board point.
    pub fn board_point(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.x as f64) / self.k, (y - self.y as f64) / self.k)
    }
}

/// The sofa mode's state in bitscomp (`App::sofa`).
#[derive(Default)]
pub struct Sofa {
    pub active: bool,
    pub front: Front,
    pub nav: Nav,
    /// The focused element's id (sofa_layout), None when the screen has none.
    pub focus: Option<String>,
    /// A footer hint under the pointer (hints are buttons, not focus targets).
    pub hover: Option<String>,
    /// The workspace shown before entering: leaving shows it again.
    pub before: Option<String>,
    pub prefs: Prefs,
    /// The keys of §6 were sent to settingsd on this connection.
    pub keys_sent: bool,
    /// Settingsd's values were there once (a pad plugged in before decides when they come).
    pub settings_seen: bool,
    /// A pad plugged in before the settings came: decided when they come, at the latest at this time.
    pub pending: Option<Duration>,
    /// GM-6d: the search screen and its keyboard (the field, the hits, the held keys).
    pub search: crate::sofa_search::Search,
    pub images: crate::sofa_draw::Images,
    /// GM-6b: Start, Bibliothek and Medien (collection, selection, scroll, play time, the music player's answer).
    pub body: crate::sofa_body::Body,
    /// GM-6c: the quick menu (volume, outputs, battery, Spiel beenden, power).
    pub quick: crate::sofa_quick::Quick,
    /// GM-6c: Einstellungen (section, settingsd's answer, the learn screen).
    pub set: crate::sofa_settings::Set,
    /// GM-6c: a question shown (Spiel beenden, Neu starten, Ausschalten): only Ja and Nein take the focus.
    pub confirm: Option<crate::sofa_quick::Confirm>,
}

impl Sofa {
    pub fn show(&self) -> Show {
        match (self.active, self.front, self.nav.quick) {
            (false, ..) => Show::None,
            (true, Front::Shell, _) => Show::Shell,
            (true, Front::Program, true) => Show::Over,
            (true, Front::Program, false) => Show::None,
        }
    }

    /// Whether the sofa layer takes the pad's buttons, the keys and the pointer (a shell or the quick menu shown).
    pub fn listens(&self) -> bool {
        self.show() != Show::None
    }
}

#[cfg(test)]
#[path = "sofa_tests.rs"]
mod tests;
