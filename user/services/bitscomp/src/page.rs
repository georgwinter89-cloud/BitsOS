//! WY-5b: the settings page `einstellungen/...` that bitscomp draws itself into a tab (`page_client` gives it a
//! window). Every key of settingsd in its group, each with the control that fits its type (WY-9: built from the
//! registry's schema in `page_schema`), and the favourites. Every change goes to settingsd (`settings_write`), so
//! its journal keeps undo and history; "Rückgängig" is settingsd's `undo`. The address only scrolls to its key and
//! lights it, as a browser does with an anchor.
//! This file: the rows, the address words, and the layout in logical pixels with its hit boxes.

use std::ops::Range;

use crate::layout::Rect;

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    /// (value, German name) of each choice, in the registry's order.
    Choice(Vec<(String, String)>),
    /// The accent: the mock-up's swatches and a hex field.
    Accent,
    /// A whole number: − and + by `step`, the value between them (a field: a click types a number).
    /// `word`: (value, German name) of a word that stands instead of a number, a pill after +.
    Number { min: i64, max: i64, step: i64, unit: String, word: Option<(String, String)> },
    Switch,
    /// A text field of at most `max` bytes; `colour`: it takes `#rrggbb`.
    Field { colour: bool, max: usize },
    /// No control, the value only; the text says why.
    Fixed(String),
    /// CL-5 (the page "KI"): pills `w` wide, (value, name) each. One of them is the value, or with `multi` the
    /// value is a list and a click puts a name on it or takes it off. `notes`: one line beside each choice
    /// (empty: none); with notes the choices stand one under the other.
    Pills { opts: Vec<(String, String)>, multi: bool, w: i32, notes: Vec<String> },
    /// CL-5: a secret. The row shows only whether one is stored; its field never shows what is typed. `clear`:
    /// a button that removes the stored one.
    Secret { clear: bool },
    /// CL-5: buttons, (name in `ctl hot`, text) each.
    Buttons(Vec<(String, String)>),
    /// DESK-LEFT: a way to another page: its "Öffnen" button opens the address (as typed into the address bar).
    Link(String),
    /// SELFOPT-1: a benchmark's last nights as a small line chart (the value is `leistung::chart_value`'s text).
    Chart,
    /// WY-SET: a card's "Mehr" line: open or closed, how many rows it holds and how many of them differ from
    /// their default. The whole row is its button.
    Fold { open: bool, count: usize, changed: usize },
    /// WY-SET: several keys in one row (a consolidation, `page_join`): each a small switch or a number chip that
    /// is typed into. The row's value is the keys' values joined by `;`.
    Subs(Vec<Sub>),
}

/// WY-SET: one key of a `Subs` row.
#[derive(Clone, Debug, PartialEq)]
pub struct Sub {
    pub key: String,
    pub label: String,
    pub unit: String,
    pub switch: bool,
}

/// WY-SET: how the section heads are drawn: above the card in capitals (the pages "KI" and "Wer hat was
/// gemacht"), as a card's title inside it (the settings tabs), or as a search result's path inside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Heads {
    #[default]
    Above,
    Title,
    Crumb,
}

/// WY-SET: a row's marks on the settings tabs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flag {
    /// Its key differs from the default (a dot after the label).
    pub changed: bool,
    /// It offers "Zurücksetzen" (the list "Nur geänderte" and the search).
    pub reset: bool,
}

/// DESK-LEFT: the Programme group's last row, the way to programme/hinzufuegen (a key settingsd never has: it only
/// names the row, so `einstellungen/programme/hinzufuegen` scrolls to it).
pub const ADD_LINK_KEY: &str = "pkg.hinzufuegen";
pub const LINK_TEXT: &str = "Öffnen";

pub fn add_programs_row() -> Row {
    Row {
        key: ADD_LINK_KEY.into(),
        kind: Kind::Link("programme/hinzufuegen".into()),
        default: String::new(),
        label: "Programme hinzufügen".into(),
        short: "Programme hinzufügen".into(),
        note: String::new(),
        confirm: false,
        stack: false,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub key: String,
    pub kind: Kind,
    pub default: String,
    /// The German description.
    pub label: String,
    /// The name in answer lines ("Akzentfarbe: Geändert").
    pub short: String,
    /// Said after the address (e.g. "wirkt nach dem nächsten Start"); empty: nothing.
    pub note: String,
    /// settingsd's safety class `confirm`: a change goes through the page's own dialog first (WY-10).
    pub confirm: bool,
    /// The controls go under the label, so the sentence under it has the row's whole width (a refusal that
    /// says what to do must not be cut).
    pub stack: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Section {
    /// A group head and its rows (indices into `Model::rows`).
    Rows(String, Range<usize>),
    Favourites,
}

/// What the page shows: the same for every page (the address only scrolls).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    pub rows: Vec<Row>,
    pub sections: Vec<Section>,
    /// WY-SET: the heads' look, one mark per row (empty: none), and the narrowest column two columns may have
    /// (0: `page_layout::COL_MIN`).
    pub heads: Heads,
    pub flags: Vec<Flag>,
    pub col_min: i32,
}

pub const FAVOURITES: &str = "desktop.favourites";
/// The accent swatches, lower case as settingsd stores colours: design v2's moss green (the default, INT-D1) first,
/// then the mock-up's (Details.dc.html) cyan, amber and violet (its mint green is too close to the moss).
pub const SWATCHES: [&str; 4] = ["#7fcfa0", "#00e5ff", "#ffb020", "#b388ff"];
pub const SCALE_STEP: i64 = 25;

/// German address words and the key words they stand for (desktop.md §1a: German words, English accepted).
const WORDS: [(&str, &str); 18] = [
    ("modus", "mode"),
    ("akzent", "accent"),
    ("skalierung", "scale"),
    ("bewegung", "motion"),
    ("effekte", "effects"),
    ("schrift", "font"),
    ("abstand", "spacing"),
    ("ecken", "radius"),
    ("leuchten", "glow"),
    ("dunkel", "dark"),
    ("hell", "light"),
    ("adresszeile", "address_bar"),
    ("favoritenleiste", "favourites_bar"),
    ("claude-leiste", "claude_sidebar"),
    ("arbeitsraumliste", "workspaces_bar"),
    ("statusleiste", "statusbar"),
    ("groesse", "size"),
    ("aufbau", "layout"),
];

/// The key prefix of a settings path after `einstellungen/` (slashes or dots): `aussehen/akzent` -> theme.accent,
/// `aufbau/statusleiste` -> theme.layout.statusbar, `favoriten` -> desktop.favourites; other words stay.
pub fn prefix_of(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '.']).filter(|s| !s.is_empty()).collect();
    let Some((first, rest)) = parts.split_first() else { return String::new() };
    let head = match *first {
        "aussehen" => "theme",
        "aufbau" => "theme.layout",
        "favoriten" => FAVOURITES,
        // PK-7a: bitspkgd's keys (sources, CPU level, space, updates; plan/spec/program-library.md section 4).
        "programme" => "pkg",
        // WY-SET: a tab without a namespace of its own names no key: the tab's top.
        "schreibtisch" | "ton" | "netz" | "assistent" | "sicherungen" | "system" if rest.is_empty() => "",
        w => w,
    };
    let mut out = head.to_string();
    for w in rest {
        out.push('.');
        out.push_str(WORDS.iter().find(|(de, _)| de == w).map_or(*w, |(_, en)| en));
    }
    out
}

/// The German address of a key (`theme.accent` -> einstellungen/aussehen/akzent).
pub fn address_of_key(key: &str) -> String {
    let de = |w: &str| WORDS.iter().find(|(_, en)| *en == w).map_or(w.to_string(), |(de, _)| de.to_string());
    if key == FAVOURITES {
        return "einstellungen/favoriten".into();
    }
    let (head, rest) = if let Some(r) = key.strip_prefix("pkg.") {
        ("einstellungen/programme", r)
    } else if let Some(r) = key.strip_prefix("theme.layout.") {
        ("einstellungen/aufbau", r)
    } else if let Some(r) = key.strip_prefix("theme.") {
        ("einstellungen/aussehen", r)
    } else {
        ("einstellungen", key)
    };
    let words: Vec<String> = rest.split('.').map(de).collect();
    format!("{head}/{}", words.join("/"))
}

/// Something on the page that reacts to the mouse. Row numbers index `Model::rows`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Undo,
    Choice(usize, usize),
    Swatch(usize),
    Hex,
    Less(usize),
    More(usize),
    /// A number row's word pill (`alles`).
    Word(usize),
    /// A row's field (a number, a colour, a text).
    Field(usize),
    Switch(usize),
    FavUp(usize),
    FavRemove(usize),
    FavField,
    FavAdd,
    /// The page itself (focuses it; the circle menu on the right button).
    Body,
    /// The buttons of the page's confirmation dialog (`page_confirm`).
    DialogCancel,
    DialogOk,
    /// PK-7a: a control of the "Programme" page (`programme_view`).
    Prog(crate::programme_view::ProgPart),
    /// CL-5: pill `o` of a `Pills` row, button `o` of a `Buttons` row (0 of a `Secret` row: its "Löschen").
    Pill(usize, usize),
    Button(usize, usize),
    /// DESK-LEFT: the "Öffnen" of a `Link` row.
    Link(usize),
    /// SYS-A: a control of the program "System" (`sys_view`).
    Sys(crate::sys_view::SysPart),
    /// WY-SET: the settings' rail: tab `i`, the search field and its ×, "Zeigen" of the changed keys; the head's
    /// filter (false: Alle, true: Nur geänderte); a card's "Mehr"; a row's "Zurücksetzen"; key `k` of a Subs row.
    Tab(usize),
    Search,
    SearchClear,
    Changed,
    Filter(bool),
    Fold(usize),
    Reset(usize),
    Sub(usize, usize),
}

/// The name of `p` in `ctl hot` (scenarios click by it): `choice=KEY=VALUE`, `switch=KEY`, `less=KEY` ...
pub fn part_name(rows: &[Row], p: Part) -> String {
    let key = |r: usize| rows.get(r).map_or("?", |r| r.key.as_str());
    match p {
        Part::Undo => "undo".into(),
        Part::Choice(r, o) => match rows.get(r).map(|r| &r.kind) {
            Some(Kind::Choice(opts)) => format!("choice={}={}", key(r), opts.get(o).map_or("?", |o| o.0.as_str())),
            _ => "choice".into(),
        },
        Part::Swatch(i) => format!("swatch={}", SWATCHES.get(i).unwrap_or(&"?")),
        Part::Hex => "hex".into(),
        Part::Less(r) => format!("less={}", key(r)),
        Part::More(r) => format!("more={}", key(r)),
        Part::Word(r) => format!("word={}", key(r)),
        Part::Field(r) => format!("field={}", key(r)),
        Part::Switch(r) => format!("switch={}", key(r)),
        Part::FavUp(i) => format!("fav-up={i}"),
        Part::FavRemove(i) => format!("fav-remove={i}"),
        Part::FavField => "fav-field".into(),
        Part::FavAdd => "fav-add".into(),
        Part::Body => "body".into(),
        Part::DialogCancel => "dialog-cancel".into(),
        Part::DialogOk => "dialog-ok".into(),
        // Named by the programme page itself (programme_view::part_name).
        Part::Prog(_) => "programme".into(),
        Part::Sys(p) => format!("system={}", crate::sys_view::part_name(p)),
        Part::Pill(r, o) => match rows.get(r).map(|r| &r.kind) {
            Some(Kind::Pills { opts, multi, .. }) => {
                format!("{}={}={}", if *multi { "toggle" } else { "choice" }, key(r), opts.get(o).map_or("?", |o| o.0.as_str()))
            }
            _ => "pill".into(),
        },
        Part::Link(r) => match rows.get(r).map(|r| &r.kind) {
            Some(Kind::Link(a)) => format!("link={a}"),
            _ => "link".into(),
        },
        Part::Tab(i) => format!("tab={}", crate::page_tabs::TABS.get(i).map_or("?", |t| t.word)),
        Part::Search => "search".into(),
        Part::SearchClear => "search-clear".into(),
        Part::Changed => "changed".into(),
        Part::Filter(on) => format!("filter={}", if on { "geaendert" } else { "alle" }),
        Part::Fold(r) => format!("fold={}", key(r).strip_prefix(crate::page_tabs::FOLD).unwrap_or("?")),
        Part::Reset(r) => format!("reset={}", key(r)),
        Part::Sub(r, k) => match rows.get(r).map(|r| &r.kind) {
            Some(Kind::Subs(s)) => format!("sub={}", s.get(k).map_or("?", |s| s.key.as_str())),
            _ => "sub".into(),
        },
        Part::Button(r, o) => match rows.get(r).map(|r| &r.kind) {
            Some(Kind::Buttons(b)) => format!("button={}={}", key(r), b.get(o).map_or("?", |b| b.0.as_str())),
            Some(Kind::Secret { .. }) => format!("button={}=clear", key(r)),
            _ => "button".into(),
        },
    }
}

/// What the layout needs to know besides the content rectangle and the model.
#[derive(Clone, Debug, PartialEq)]
pub struct Inputs {
    pub favourites: usize,
    pub scroll: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub content: Rect,
    pub narrow: bool,
    /// The head that does not scroll; `scrolled`: something lies hidden under it.
    pub head: Rect,
    pub scrolled: bool,
    pub title: Rect,
    pub sub: Rect,
    pub msg: Rect,
    /// Section heads: (text, rect).
    pub heads: Vec<(String, Rect)>,
    /// Cards behind the rows of a group.
    pub cards: Vec<Rect>,
    /// One per model row: its rect, and whether it ends its group (no line under it).
    pub rows: Vec<Rect>,
    pub last: Vec<bool>,
    /// The favourites' rows, then the row with the field to add one.
    pub favs: Vec<Rect>,
    pub fav_add_row: Rect,
    pub parts: Vec<(Part, Rect)>,
    /// The page's full height (for the scroll limit).
    pub total: i32,
    /// WY-SET: per row, the room left of its controls for its "Zurücksetzen" (empty: none anywhere).
    pub extra: Vec<i32>,
    /// WY-SET: how the heads are drawn, and the settings' rail left of the page (None: no rail).
    pub heads_look: Heads,
    pub rail: Option<crate::page_rail::Rail>,
}

pub const ROW_H: i32 = 68;
pub const ROW_H_NARROW: i32 = 108;
pub const FAV_H: i32 = 48;
pub const CTL_H: i32 = 36;
pub const OPT_W: i32 = 112;
pub const SWATCH: i32 = 36;
pub const HEX_W: i32 = 120;
pub const FIELD_W: i32 = 260;
pub const NUM_W: i32 = 84;
/// The width of a value without a control.
pub const VALUE_W: i32 = 260;
pub const UNDO_W: i32 = 150;
/// SELFOPT-1: a history chart's width.
pub const CHART_W: i32 = 300;
pub(crate) const HEAD_H: i32 = 18;
/// The fixed head of the page.
pub const HEAD_AREA: i32 = 136;
pub(crate) const MAX_W: i32 = 920;
/// Below this content width the controls go under their label.
pub(crate) const NARROW: i32 = 640;
/// Controls go under the label also when they would leave it less than this, or less than its text needs
/// (estimated: LABEL_CHAR per character of the 15 px label font; the layout knows no font).
const MIN_LABEL: i32 = 240;
const LABEL_CHAR: i32 = 8;
/// Gap between lines of wrapped choices.
const LINE_GAP: i32 = 8;

/// The width of a choice's pills: the longest name's (at least OPT_W).
fn choice_w(opts: &[(String, String)]) -> i32 {
    pill_w(opts.iter().map(|(_, n)| n.as_str()))
}

fn per_line_of(avail: i32, w: i32) -> usize {
    (avail.saturating_add(4) / (w.max(1) + 4)).max(1) as usize
}

/// WY-SET: the width of a Subs row's chip, of a row's "Zurücksetzen" and the height of a "Mehr" line.
pub const SUB_W: i32 = 190;
pub const RESET_W: i32 = 130;
pub const FOLD_H: i32 = 52;

/// CL-5: the width of a secret's field and of a button with `text` (estimated like the labels).
pub const SECRET_W: i32 = 150;
pub fn button_w(text: &str) -> i32 {
    ((text.chars().count() as i32) * 9 + 36).clamp(96, 260)
}

/// CL-5: the width of pills that hold the longest of `names`.
pub fn pill_w<'a>(names: impl Iterator<Item = &'a str>) -> i32 {
    (names.map(|n| n.chars().count() as i32).max().unwrap_or(0) * 9 + 32).clamp(OPT_W, 260)
}

/// Controls of `row` (number `r`) placed from `x` at `y` within width `avail` (logical pixels). Choices wrap.
pub(crate) fn controls(row: &Row, r: usize, x: i32, y: i32, avail: i32) -> Vec<(Part, Rect)> {
    let at = |dx: i32, w: i32| Rect { x: x + dx, y, w, h: CTL_H };
    match &row.kind {
        Kind::Choice(opts) => {
            // WY-SET: pills as wide as the longest name needs ("Tiefe Schlafzustände" was cut at 112).
            let w = choice_w(opts).min(avail);
            let n = per_line_of(avail, w);
            (0..opts.len())
                .map(|o| {
                    let (line, col) = ((o / n) as i32, (o % n) as i32);
                    (Part::Choice(r, o), Rect { x: x + col * (w + 4), y: y + line * (CTL_H + LINE_GAP), w, h: CTL_H })
                })
                .collect()
        }
        Kind::Accent => {
            let mut v: Vec<(Part, Rect)> =
                (0..SWATCHES.len()).map(|i| (Part::Swatch(i), at(i as i32 * (SWATCH + 8), SWATCH))).collect();
            v.push((Part::Hex, at(SWATCHES.len() as i32 * (SWATCH + 8) + 4, HEX_W)));
            v
        }
        Kind::Number { word, .. } => {
            let mut v = vec![
                (Part::Less(r), at(0, CTL_H)),
                (Part::Field(r), at(CTL_H + 4, NUM_W)),
                (Part::More(r), at(CTL_H + 4 + NUM_W + 4, CTL_H)),
            ];
            if word.is_some() {
                v.push((Part::Word(r), at(CTL_H + 4 + NUM_W + 4 + CTL_H + 8, OPT_W)));
            }
            v
        }
        Kind::Switch => vec![(Part::Switch(r), Rect { x, y: y + 4, w: 52, h: 28 })],
        Kind::Field { colour: true, .. } => vec![(Part::Field(r), at(SWATCH + 8, HEX_W))],
        Kind::Field { colour: false, .. } => vec![(Part::Field(r), at(0, FIELD_W.min(avail)))],
        Kind::Fixed(_) | Kind::Chart | Kind::Fold { .. } => Vec::new(),
        Kind::Subs(subs) => {
            let n = per_line_of(avail, SUB_W);
            (0..subs.len())
                .map(|k| {
                    let (line, col) = ((k / n) as i32, (k % n) as i32);
                    (Part::Sub(r, k), Rect { x: x + col * (SUB_W + 8), y: y + line * (CTL_H + LINE_GAP), w: SUB_W.min(avail), h: CTL_H })
                })
                .collect()
        }
        Kind::Pills { opts, w, notes, .. } => {
            let n = if notes.is_empty() { per_line_of(avail, *w) } else { 1 };
            (0..opts.len())
                .map(|o| {
                    let (line, col) = ((o / n) as i32, (o % n) as i32);
                    (Part::Pill(r, o), Rect { x: x + col * (w + 4), y: y + line * (CTL_H + LINE_GAP), w: (*w).min(avail), h: CTL_H })
                })
                .collect()
        }
        Kind::Secret { clear } => {
            let mut v = vec![(Part::Field(r), at(0, SECRET_W.min(avail)))];
            if *clear {
                v.push((Part::Button(r, 0), at(SECRET_W + 8, button_w("Löschen"))));
            }
            v
        }
        Kind::Link(_) => vec![(Part::Link(r), at(0, button_w(LINK_TEXT)))],
        Kind::Buttons(b) => {
            let mut dx = 0;
            let mut v = Vec::new();
            for (o, (_, text)) in b.iter().enumerate() {
                let w = button_w(text);
                v.push((Part::Button(r, o), at(dx, w)));
                dx += w + 8;
            }
            v
        }
    }
}

/// The width of `row`'s controls in width `avail`.
pub fn controls_width(row: &Row, avail: i32) -> i32 {
    match row.kind {
        Kind::Fixed(_) => VALUE_W.min(avail),
        Kind::Chart => CHART_W.min(avail),
        Kind::Fold { .. } => 0,
        _ => controls(row, 0, 0, 0, avail).iter().map(|(_, b)| b.x + b.w).max().unwrap_or(0),
    }
}

/// Lines of `row`'s controls in width `avail`.
fn lines(row: &Row, avail: i32) -> i32 {
    match &row.kind {
        Kind::Choice(o) if !o.is_empty() => o.len().div_ceil(per_line_of(avail, choice_w(o).min(avail))) as i32,
        Kind::Pills { opts, notes, .. } if !notes.is_empty() => opts.len().max(1) as i32,
        Kind::Pills { opts, w, .. } if !opts.is_empty() => opts.len().div_ceil(per_line_of(avail, *w)) as i32,
        Kind::Subs(s) if !s.is_empty() => s.len().div_ceil(per_line_of(avail, SUB_W)) as i32,
        _ => 1,
    }
}

/// Where `row`'s controls start in its rect `b` of page width `w`: (x, y, avail); and the row's height.
pub fn control_place(row: &Row, x: i32, y: i32, w: i32, narrow: bool) -> (i32, i32, i32, i32) {
    control_place_beside(row, x, y, w, narrow, 0)
}

/// `control_place` with `extra` logical pixels more beside the controls (a row's "Zurücksetzen" left of them).
pub fn control_place_beside(row: &Row, x: i32, y: i32, w: i32, narrow: bool, extra: i32) -> (i32, i32, i32, i32) {
    if let Kind::Fold { .. } = row.kind {
        return (x + w - 20, y, 0, FOLD_H);
    }
    let wide = controls_width(row, i32::MAX) + extra;
    let label = (row.label.chars().count() as i32).saturating_mul(LABEL_CHAR).max(MIN_LABEL);
    let noted = matches!(&row.kind, Kind::Pills { notes, .. } if !notes.is_empty());
    if !narrow && !noted && !row.stack && wide <= w - 64 - label {
        (x + w - 20 - wide + extra, y + (ROW_H - CTL_H) / 2, wide - extra, ROW_H)
    } else {
        let avail = (w - 40).max(1);
        (x + 20, y + 58, avail, ROW_H_NARROW + (lines(row, avail) - 1) * (CTL_H + LINE_GAP))
    }
}

pub use crate::page_layout::layout;

impl Layout {
    pub fn hit(&self, x: f64, y: f64) -> Option<Part> {
        if !self.content.contains(x, y) {
            return None;
        }
        Some(self.parts.iter().find(|(_, b)| b.contains(x, y)).map_or(Part::Body, |(p, _)| *p))
    }

    pub fn part(&self, p: Part) -> Option<Rect> {
        self.parts.iter().find(|(q, _)| *q == p).map(|(_, b)| *b)
    }

    /// The largest scroll that still shows the page's end.
    pub fn max_scroll(&self) -> i32 {
        (self.total + 24 - self.content.h).max(0)
    }

    /// The scroll that shows the first row of `prefix` (unchanged when it is already in view): a page opened
    /// at an address shows that setting, as a browser scrolls to an anchor.
    pub fn scroll_to(&self, prefix: &str, rows: &[Row], scroll: i32) -> Option<i32> {
        let r = self.row_at(prefix, rows)?;
        let top = r.y + scroll - self.content.y;
        let (from, h) = (HEAD_AREA + 16, self.content.h - HEAD_AREA - 16);
        if top - scroll >= from && top - scroll + r.h <= from + h {
            return Some(scroll);
        }
        Some((top - from).clamp(0, self.max_scroll()))
    }

    /// The rect of the first row whose key is `prefix` or lies under it (the favourites' head for theirs).
    pub fn row_at(&self, prefix: &str, rows: &[Row]) -> Option<Rect> {
        if prefix.is_empty() {
            return None;
        }
        let hit = |k: &str| k == prefix || k.starts_with(&format!("{prefix}."));
        rows.iter()
            .position(|r| hit(&r.key))
            .and_then(|i| self.rows.get(i).copied())
            .or_else(|| hit(FAVOURITES).then(|| self.heads.iter().find(|(n, _)| n.eq_ignore_ascii_case("FAVORITEN")).map(|(_, r)| *r)).flatten())
    }

    /// The y (unscrolled, from the content's top) of the first row of `prefix`.
    #[cfg(test)]
    pub fn row_top(&self, prefix: &str, rows: &[Row], scroll: i32) -> Option<i32> {
        Some(self.row_at(prefix, rows)?.y + scroll - self.content.y)
    }
}

/// The value after one step of − or + (snapped to the step, in range).
pub fn number_step(now: i64, up: bool, min: i64, max: i64, step: i64) -> i64 {
    let step = step.max(1);
    let s = if up {
        now.div_euclid(step).saturating_add(1).saturating_mul(step)
    } else {
        now.saturating_add(step - 1).div_euclid(step).saturating_sub(1).saturating_mul(step)
    };
    s.clamp(min, max.max(min))
}

/// A number typed into a row's field (its unit may follow), or the German reason it is not one.
pub fn number(text: &str, min: i64, max: i64, unit: &str) -> Result<i64, String> {
    let t = text.trim();
    let t = t.strip_suffix(unit).filter(|_| !unit.is_empty()).unwrap_or(t).trim();
    let range = format!("eine ganze Zahl von {min} bis {max}{}", if unit.is_empty() { String::new() } else { format!(" {unit}") });
    match t.parse::<i64>() {
        Ok(n) if (min..=max).contains(&n) => Ok(n),
        _ => Err(format!("„{}“ passt nicht: {range}", text.trim())),
    }
}

/// `text` as a colour settingsd takes (`#rrggbb`, lower case; the # may be left out).
pub fn colour(text: &str) -> Option<String> {
    let t = text.trim().trim_start_matches('#').to_lowercase();
    (t.len() == 6 && t.chars().all(|c| c.is_ascii_hexdigit())).then(|| format!("#{t}"))
}

/// The favourites list without entry `i` / with `i` one place up / with `a` added (no duplicates, no commas).
pub fn favs_without(v: &[String], i: usize) -> Vec<String> {
    v.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, s)| s.clone()).collect()
}

pub fn favs_up(v: &[String], i: usize) -> Vec<String> {
    let mut v = v.to_vec();
    if i > 0 && i < v.len() {
        v.swap(i - 1, i);
    }
    v
}

pub fn favs_with(v: &[String], a: &str) -> Option<Vec<String>> {
    let a = a.trim().replace(',', " ");
    let a = a.trim();
    if a.is_empty() || v.iter().any(|s| s == a) {
        return None;
    }
    let mut v = v.to_vec();
    v.push(a.to_string());
    Some(v)
}

#[cfg(test)]
#[path = "page_tests.rs"]
pub(crate) mod tests;
