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
}

pub const FAVOURITES: &str = "desktop.favourites";
/// The mock-up's accent swatches (Details.dc.html), lower case as settingsd stores colours.
pub const SWATCHES: [&str; 4] = ["#00e5ff", "#ffb020", "#b388ff", "#3ddc97"];
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
    let (head, rest) = if let Some(r) = key.strip_prefix("theme.layout.") {
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
const HEAD_H: i32 = 18;
/// The fixed head of the page.
pub const HEAD_AREA: i32 = 136;
const MAX_W: i32 = 920;
/// Below this content width the controls go under their label.
const NARROW: i32 = 640;
/// Controls go under the label also when they would leave it less than this, or less than its text needs
/// (estimated: LABEL_CHAR per character of the 15 px label font; the layout knows no font).
const MIN_LABEL: i32 = 240;
const LABEL_CHAR: i32 = 8;
/// Gap between lines of wrapped choices.
const LINE_GAP: i32 = 8;

/// Choices per line in width `avail` (at least one).
fn per_line(avail: i32) -> usize {
    (avail.saturating_add(4) / (OPT_W + 4)).max(1) as usize
}

/// Controls of `row` (number `r`) placed from `x` at `y` within width `avail` (logical pixels). Choices wrap.
fn controls(row: &Row, r: usize, x: i32, y: i32, avail: i32) -> Vec<(Part, Rect)> {
    let at = |dx: i32, w: i32| Rect { x: x + dx, y, w, h: CTL_H };
    match &row.kind {
        Kind::Choice(opts) => {
            let n = per_line(avail);
            (0..opts.len())
                .map(|o| {
                    let (line, col) = ((o / n) as i32, (o % n) as i32);
                    (Part::Choice(r, o), Rect { x: x + col * (OPT_W + 4), y: y + line * (CTL_H + LINE_GAP), w: OPT_W, h: CTL_H })
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
        Kind::Fixed(_) => Vec::new(),
    }
}

/// The width of `row`'s controls in width `avail`.
pub fn controls_width(row: &Row, avail: i32) -> i32 {
    match row.kind {
        Kind::Fixed(_) => VALUE_W.min(avail),
        _ => controls(row, 0, 0, 0, avail).iter().map(|(_, b)| b.x + b.w).max().unwrap_or(0),
    }
}

/// Lines of `row`'s controls in width `avail`.
fn lines(row: &Row, avail: i32) -> i32 {
    match &row.kind {
        Kind::Choice(o) if !o.is_empty() => o.len().div_ceil(per_line(avail)) as i32,
        _ => 1,
    }
}

/// Where `row`'s controls start in its rect `b` of page width `w`: (x, y, avail); and the row's height.
pub fn control_place(row: &Row, x: i32, y: i32, w: i32, narrow: bool) -> (i32, i32, i32, i32) {
    let wide = controls_width(row, i32::MAX);
    let label = (row.label.chars().count() as i32).saturating_mul(LABEL_CHAR).max(MIN_LABEL);
    if !narrow && wide <= w - 64 - label {
        (x + w - 20 - wide, y + (ROW_H - CTL_H) / 2, wide, ROW_H)
    } else {
        let avail = (w - 40).max(1);
        (x + 20, y + 58, avail, ROW_H_NARROW + (lines(row, avail) - 1) * (CTL_H + LINE_GAP))
    }
}

/// The page of model `m` in content rectangle `c`, scrolled by `i.scroll` logical pixels.
pub fn layout(c: Rect, m: &Model, i: &Inputs) -> Layout {
    let pad = (c.w / 24).clamp(16, 40);
    let w = (c.w - 2 * pad).clamp(200, MAX_W);
    let x = c.x + pad;
    let narrow = w < NARROW;
    // The head (title, address, answer line, Rückgängig) stays; the rest scrolls below it.
    let mut y = c.y + 24;
    let title = Rect { x, y, w: w - UNDO_W - 12, h: 40 };
    let undo = Rect { x: x + w - UNDO_W, y: y + 2, w: UNDO_W, h: CTL_H };
    y += 46;
    let sub = Rect { x, y, w, h: 18 };
    y += 24;
    let msg = Rect { x, y, w, h: 20 };
    let head = Rect { x: c.x, y: c.y, w: c.w, h: HEAD_AREA };
    let mut parts = Vec::new();
    let mut y = c.y + HEAD_AREA + 16 - i.scroll;
    let (mut heads, mut cards) = (Vec::new(), Vec::new());
    let mut rows = vec![Rect::default(); m.rows.len()];
    let mut last = vec![false; m.rows.len()];
    let mut favs = Vec::new();
    let mut fav_add_row = Rect::default();
    for s in &m.sections {
        match s {
            Section::Rows(name, range) => {
                let range = range.start.min(m.rows.len())..range.end.min(m.rows.len());
                if range.is_empty() {
                    continue;
                }
                heads.push((name.clone(), Rect { x, y, w, h: HEAD_H }));
                y += HEAD_H + 10;
                let top = y;
                for r in range.clone() {
                    let row = &m.rows[r];
                    let (cx, cy, avail, h) = control_place(row, x, y, w, narrow);
                    parts.extend(controls(row, r, cx, cy, avail));
                    rows[r] = Rect { x, y, w, h };
                    last[r] = r + 1 == range.end;
                    y += h;
                }
                cards.push(Rect { x, y: top, w, h: y - top });
                y += 28;
            }
            Section::Favourites => {
                heads.push(("FAVORITEN".into(), Rect { x, y, w, h: HEAD_H }));
                y += HEAD_H + 10;
                let top = y;
                for f in 0..i.favourites {
                    let b = Rect { x, y, w, h: FAV_H };
                    let bx = x + w - 16 - 32;
                    parts.push((Part::FavRemove(f), Rect { x: bx, y: y + 8, w: 32, h: 32 }));
                    if f > 0 {
                        parts.push((Part::FavUp(f), Rect { x: bx - 36, y: y + 8, w: 32, h: 32 }));
                    }
                    favs.push(b);
                    y += FAV_H;
                }
                fav_add_row = Rect { x, y, w, h: FAV_H + 12 };
                let add_w = 130;
                parts.push((Part::FavField, Rect { x: x + 16, y: y + 12, w: w - 32 - add_w - 8, h: CTL_H }));
                parts.push((Part::FavAdd, Rect { x: x + w - 16 - add_w, y: y + 12, w: add_w, h: CTL_H }));
                y += FAV_H + 12 + 12;
                cards.push(Rect { x, y: top, w, h: y - top });
                y += 28;
            }
        }
    }
    // Only what can be seen can be hit.
    let below = c.y + HEAD_AREA;
    parts.retain(|(_, b)| b.y >= below && b.y + b.h <= c.y + c.h);
    if undo.y + undo.h <= c.y + c.h {
        parts.insert(0, (Part::Undo, undo));
    }
    let total = y + i.scroll - c.y;
    Layout { content: c, narrow, head, scrolled: i.scroll > 0, title, sub, msg, heads, cards, rows, last, favs, fav_add_row, parts, total }
}

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
            .or_else(|| hit(FAVOURITES).then(|| self.heads.iter().find(|(n, _)| n == "FAVORITEN").map(|(_, r)| *r)).flatten())
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
