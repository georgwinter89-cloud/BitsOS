//! GM-6a: where everything of the sofa screens lies, in board px (the 1920x1080 boards of
//! plan/spec/sofa-v1/project, sofa-mode.md §2), pure. The focus model walks these elements (`sofa_focus`), the look
//! files draw on them and the mouse hits them, so the three never disagree. `bw` x `bh` is the board size of the
//! output (1920x1080 at 16:9; the longer side grows otherwise, sofa.rs `Frame`). Texts are measured by the caller
//! (the real fonts in the shell, a rough rule in the tests).

use crate::layout::Rect;
use crate::sofa_focus::{Area, Item, Screen};

pub const HEADER_H: i32 = 96;
pub const FOOTER_H: i32 = 72;
pub const PAD: i32 = 64;
/// The quick menu's panel width.
pub const QUICK_W: i32 = 620;

/// The text styles the layout measures (board px sizes of the boards).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Text {
    /// "BitsOS" beside the mark: Bricolage 28 600.
    Word,
    /// A nav pill: Figtree 20 600.
    Pill,
    /// The status words: JetBrains Mono 16.
    Status,
    /// The clock: JetBrains Mono 24 500.
    Clock,
    /// "Menü": Figtree 18 600.
    Menu,
    /// A footer hint's words: Figtree 17.
    Hint,
    /// A footer hint's boxed sign (LB RB, HOME): Figtree 14 700.
    Sign,
    /// A section title: Bricolage 34 600.
    Section,
    /// A link or button text: Figtree 18-20 600.
    Button,
    /// The hero's buttons: Figtree 22 700 (GM-6b).
    HeroButton,
    /// GM-6c: an output chip of the quick menu: Figtree 17 600.
    Chip,
}

pub type Measure<'a> = &'a dyn Fn(&str, Text) -> i32;

#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    /// The pills, the search button and Menü (the focusable ones).
    pub items: Vec<Item>,
    pub mark: Rect,
    pub word: Rect,
    /// The status words (network, volume) with their places.
    pub status: Vec<(Rect, String)>,
    pub clock: Rect,
}

/// The header of `screen` (none on the search screen): the mark and "BitsOS", the pills, then from the right Menü,
/// the status (clock last) and the search button; the library screens keep only the clock (§2).
pub fn header(bw: i32, screen: Screen, status: &[String], clock: &str, m: Measure) -> Header {
    let mark = Rect { x: PAD, y: 28, w: 40, h: 40 };
    let word = Rect { x: PAD + 40 + 14, y: 30, w: m("BitsOS", Text::Word), h: 36 };
    let mut items = Vec::new();
    let mut x = word.x + word.w + 48;
    for s in Screen::PILLS {
        let w = m(s.title(), Text::Pill) + 48;
        items.push(Item::new(format!("pill:{}", s.word()), Rect { x, y: 23, w, h: 50 }, Area::Header));
        x += w + 8;
    }
    let menu_w = 22 + 22 + 10 + 22 + m("Menü", Text::Menu);
    let menu = Rect { x: bw - PAD - menu_w, y: 22, w: menu_w, h: 52 };
    let lib = matches!(screen, Screen::Bibliothek | Screen::Medien);
    let cw = m(clock, Text::Clock);
    let clock = Rect { x: menu.x - 48 - cw, y: 32, w: cw, h: 32 };
    let mut left = clock.x;
    let mut words = Vec::new();
    if !lib {
        for s in status.iter().rev() {
            let w = m(s, Text::Status);
            left -= 28 + w;
            words.push((Rect { x: left, y: 36, w, h: 24 }, s.clone()));
        }
        words.reverse();
        items.push(Item::new("search", Rect { x: left - 48 - 52, y: 22, w: 52, h: 52 }, Area::Header));
    }
    items.push(Item::new("menu", menu, Area::Header));
    Header { items, mark, word, status: words, clock }
}

/// One footer hint: the button sign, the action word and the keyboard and mouse ways (§2 footer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hint {
    pub id: &'static str,
    pub sign: &'static str,
    /// A round sign (A, B, Y) or a rounded box (LB RB, HOME, ◀ ▶).
    pub round: bool,
    /// Filled with the accent (A).
    pub filled: bool,
    pub word: &'static str,
    pub ways: &'static str,
    /// At the footer's right end (HOME on Start).
    pub right: bool,
}

const fn hint(id: &'static str, sign: &'static str, round: bool, word: &'static str, ways: &'static str) -> Hint {
    Hint { id, sign, round, filled: false, word, ways, right: false }
}

const A_OPEN: Hint = Hint { filled: true, ..hint("hint:a", "A", true, "Öffnen", "· Enter · Klick") };
const A_PLAY: Hint = Hint { filled: true, ..hint("hint:a", "A", true, "Spielen", "· Enter · Doppelklick") };
const B_BACK: Hint = hint("hint:b", "B", true, "Zurück", "· Esc · Rechtsklick");
const Y_FIND: Hint = hint("hint:y", "Y", true, "Suchen", "· Tippen");
const HOME: Hint = Hint { right: true, ..hint("hint:home", "HOME", false, "Zum Desktop", "· Win+Esc") };
const LBRB: Hint = hint("hint:lbrb", "LB RB", false, "Sammlung wechseln", "· Bild auf/ab · Mausrad");
const VALUE: Hint = hint("hint:lr", "◀ ▶", false, "Wert ändern", "· Pfeiltasten · Klick");
const B_ESC: Hint = hint("hint:b", "B", true, "Zurück", "· Esc");
/// GM-6c: the newest change made here goes back (settingsd's undo).
const X_UNDO: Hint = hint("hint:x", "X", true, "Rückgängig", "· Rücktaste");

/// The hints of each screen, as the boards show them (none on the search screen: the keyboard has its own line).
pub fn hints(screen: Screen) -> &'static [Hint] {
    match screen {
        Screen::Start => &[A_OPEN, B_BACK, Y_FIND, HOME],
        Screen::Bibliothek | Screen::Medien => &[A_PLAY, B_BACK, LBRB],
        Screen::Einstellungen => &[VALUE, B_ESC, X_UNDO],
        Screen::Suche => &[],
    }
}

/// A placed hint: the whole button, its sign.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    pub rect: Rect,
    pub sign: Rect,
    pub hint: Hint,
}

/// The footer's hints at their places: from the left with 44 px between, the right ones from the right end.
pub fn footer(bw: i32, bh: i32, screen: Screen, m: Measure) -> Vec<Placed> {
    let y = bh - FOOTER_H + (FOOTER_H - 30) / 2;
    let size = |h: &Hint| {
        let sign = if h.round { 30 } else { m(h.sign, Text::Sign) + 20 };
        (sign, sign + 10 + m(h.word, Text::Hint) + 10 + m(h.ways, Text::Hint))
    };
    let mut out = Vec::new();
    let mut x = PAD;
    for h in hints(screen).iter().filter(|h| !h.right) {
        let (s, w) = size(h);
        out.push(Placed { rect: Rect { x, y, w, h: 30 }, sign: Rect { x, y, w: s, h: 30 }, hint: *h });
        x += w + 44;
    }
    let mut r = bw - PAD;
    for h in hints(screen).iter().filter(|h| h.right) {
        let (s, w) = size(h);
        r -= w;
        out.push(Placed { rect: Rect { x: r, y, w, h: 30 }, sign: Rect { x: r, y, w: s, h: 30 }, hint: *h });
        r -= 44;
    }
    out
}

/// What the start screen holds (GM-6b): the music card (only with a music player), `recent` tiles of Zuletzt
/// gespielt, and whether the hero shows a game (Spielen and Details) or the empty state (one button).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartShape {
    pub music: bool,
    pub recent: usize,
    pub game: bool,
}

pub const HERO: Rect = Rect { x: PAD, y: HEADER_H + 48, w: 1180, h: 470 };
/// The hero's text and buttons: 56 from its left and 52 from its bottom (board Main).
pub const HERO_IN: i32 = 56;
pub const RECENT_MAX: usize = 4;

/// The hero's buttons, drawn on it: Spielen and Details, or the empty state's "Bibliothek öffnen" (one).
pub fn hero_buttons(game: bool, m: Measure) -> Vec<(&'static str, Rect)> {
    let y = HERO.y + HERO.h - 52 - 64;
    let x = HERO.x + HERO_IN;
    if !game {
        return vec![(EMPTY_HERO, Rect { x, y, w: 40 + m(EMPTY_HERO, Text::HeroButton) + 40, h: 64 })];
    }
    let pw = 40 + 20 + 12 + m("Spielen", Text::HeroButton) + 40;
    let dw = 32 + m("Details", Text::HeroButton) + 32;
    vec![("Spielen", Rect { x, y, w: pw, h: 64 }), ("Details", Rect { x: x + pw + 16, y, w: dw, h: 64 })]
}

pub const EMPTY_HERO: &str = "Bibliothek öffnen";

/// The start screen's body (§2.1): the hero tile (A on it plays) with its Details button on it (a nested element:
/// sofa_focus reaches it by Right from the hero), the YouTube and music cards, "Alle ansehen", the tiles of
/// Zuletzt gespielt (one row: at most 4) and the dashed "Steam öffnen".
pub fn start(bw: i32, shape: StartShape, m: Measure) -> Vec<Item> {
    let top = HERO.y;
    let cx = HERO.x + HERO.w + 32;
    let cw = bw - PAD - cx;
    let ch = (470 - 32) / 2;
    let mut v = vec![Item::new("hero", HERO, Area::Content)];
    if let Some((_, r)) = hero_buttons(shape.game, m).get(1) {
        v.push(Item::new("hero:details", *r, Area::Content));
    }
    v.push(Item::new("card:youtube", Rect { x: cx, y: top, w: cw, h: ch }, Area::Content));
    if shape.music {
        v.push(Item::new("card:musik", Rect { x: cx, y: top + ch + 32, w: cw, h: ch }, Area::Content));
    }
    let sy = top + 470 + 44;
    let link = PAD + m("Zuletzt gespielt", Text::Section) + 20;
    v.push(Item::new("alle", Rect { x: link, y: sy, w: m("Alle ansehen", Text::Button), h: 38 }, Area::Content));
    let ty = sy + 38 + 20;
    let n = shape.recent.min(RECENT_MAX);
    for i in 0..=n {
        let r = Rect { x: PAD + i as i32 * (336 + 28), y: ty, w: 336, h: 228 };
        v.push(Item::new(if i == n { "steam".to_string() } else { format!("tile:{i}") }, r, Area::Content));
    }
    v
}

/// Where the library's parts lie (§2.2, board Bibliothek), board px.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LibGeom {
    /// The left column (its line at the right).
    pub side: Rect,
    pub title: Rect,
    /// The grid's first tile.
    pub grid: Rect,
    /// The rows of tiles that fit above the selection bar.
    pub rows: usize,
    pub bar: Rect,
}

pub const COLS: usize = 5;
const GAP: i32 = 28;

/// The board's grid (330 px tiles, two rows, the bar) is 60 px taller than its 912 px between the bars: the
/// tiles get shorter (down to 280) so its two rows fit, else fewer rows show and the rest scrolls.
pub fn lib_geom(bw: i32, bh: i32) -> LibGeom {
    let side = Rect { x: 0, y: HEADER_H, w: 360, h: bh - HEADER_H - FOOTER_H };
    let x0 = 360 + PAD;
    let gw = bw - x0 - PAD;
    let tw = (gw - (COLS as i32 - 1) * GAP) / COLS as i32;
    let title = Rect { x: x0, y: HEADER_H + 40, w: gw, h: 52 };
    let gy = title.y + title.h + GAP;
    let bar = Rect { x: x0, y: bh - FOOTER_H - 40 - 96, w: gw, h: 96 };
    let room = bar.y - GAP - gy;
    let two = (room - GAP) / 2;
    let (th, rows) = if room >= 2 * 330 + GAP {
        (330, ((room + GAP) / (330 + GAP)) as usize)
    } else if two >= 280 {
        (two, 2)
    } else {
        (room.clamp(120, 330), 1)
    };
    LibGeom { side, title, grid: Rect { x: x0, y: gy, w: tw, h: th }, rows, bar }
}

/// Tile `i` of the grid when the grid starts at row `first`.
pub fn tile_rect(g: &LibGeom, i: usize, first: usize) -> Rect {
    let (r, c) = ((i / COLS - first) as i32, (i % COLS) as i32);
    Rect { x: g.grid.x + c * (g.grid.w + GAP), y: g.grid.y + r * (g.grid.h + GAP), ..g.grid }
}

/// The library (§2.2; Medien the same on the media collections): `colls` items in the left column and the sort
/// button at its foot, the grid of 5 columns of `tiles` from row `first` (the rows that fit; the rest scrolls),
/// the bar's Spielen and In Steam öffnen.
pub fn library(bw: i32, bh: i32, colls: usize, tiles: usize, first: usize, m: Measure) -> Vec<Item> {
    let g = lib_geom(bw, bh);
    let side_w = 360 - PAD - 24;
    let mut v: Vec<Item> = (0..colls).map(|i| Item::new(format!("coll:{i}"), Rect { x: PAD, y: HEADER_H + 40 + 26 + i as i32 * 66, w: side_w, h: 60 }, Area::Side)).collect();
    v.push(Item::new("sort", Rect { x: PAD, y: bh - FOOTER_H - 40 - 56, w: side_w, h: 56 }, Area::Side));
    let from = first * COLS;
    for i in from..tiles.min(from + g.rows * COLS) {
        v.push(Item::new(format!("tile:{i}"), tile_rect(&g, i, first), Area::Content));
    }
    let bar = g.bar;
    let sw = m("In Steam öffnen", Text::Button) + 52;
    let pw = m("Spielen", Text::Button) + 64;
    let steam = Rect { x: bar.x + bar.w - 24 - sw, y: bar.y + 20, w: sw, h: 56 };
    v.push(Item::new("play", Rect { x: steam.x - 16 - pw, y: bar.y + 20, w: pw, h: 56 }, Area::Content));
    v.push(Item::new("steam-open", steam, Area::Content));
    v
}

/// The sections of Einstellungen and how many rows each has without a pad (§2.5; Konten is text only; Steuerung has
/// one row per pad, sofa_settings.rs).
pub const SECTIONS: [(&str, usize); 6] = [("Bild und Ton", 4), ("Steuerung", 3), ("Netz", 3), ("Sofa-Modus", 3), ("Konten", 0), ("System", 3)];

/// Einstellungen (§2.5): the six sections in the left column, the rows of `section` (`rows` of them) at the right.
pub fn settings(bw: i32, rows: usize) -> Vec<Item> {
    let mut v: Vec<Item> = (0..SECTIONS.len()).map(|i| Item::new(format!("section:{i}"), Rect { x: PAD, y: HEADER_H + 40 + i as i32 * 70, w: 420 - PAD - 24, h: 64 }, Area::Side)).collect();
    let x = 420 + PAD;
    let w = (1200 - 2 * PAD).min(bw - x - PAD);
    for j in 0..rows {
        v.push(Item::new(format!("row:{j}"), Rect { x, y: HEADER_H + 40 + 62 + 18 + j as i32 * (96 + 18), w, h: 96 }, Area::Content));
    }
    v
}

/// Where the quick menu's parts lie (board px), for the look.
pub const QUICK_RUN_H: i32 = 180;
/// The volume row's top below the running card (its title line; the slider 44 px under it).
pub fn quick_volume_y(running: bool) -> i32 {
    48 + 52 + 28 + if running { QUICK_RUN_H + 28 } else { 0 }
}

/// The quick menu's panel (§2.4): Weiterspielen and Spiel beenden while a program runs, the volume slider and
/// the output chips (their labels), the four tiles, and at the bottom Zum normalen Desktop, Neu starten,
/// Ausschalten.
pub fn quick(bw: i32, bh: i32, running: bool, outputs: &[&str], m: Measure) -> Vec<Item> {
    let x = bw - QUICK_W + 48;
    let w = QUICK_W - 96;
    let mut y = 48 + 52 + 28;
    let mut v = Vec::new();
    if running {
        let bwid = (w - 48 - 12) / 2;
        v.push(Item::new("q:resume", Rect { x: x + 24, y: y + 98, w: bwid, h: 60 }, Area::Panel));
        v.push(Item::new("q:end", Rect { x: x + 24 + bwid + 12, y: y + 98, w: bwid, h: 60 }, Area::Panel));
        y += 180 + 28;
    }
    v.push(Item::new("q:volume", Rect { x, y: y + 44, w, h: 56 }, Area::Panel));
    let mut cx = x;
    for (i, label) in outputs.iter().enumerate() {
        let cw = m(label, Text::Chip) + 40;
        v.push(Item::new(format!("q:out:{i}"), Rect { x: cx, y: y + 114, w: cw, h: 48 }, Area::Panel));
        cx += cw + 10;
    }
    y += 114 + 48 + 28;
    let tw = (w - 14) / 2;
    for (i, id) in ["q:wlan", "q:pad", "q:screen", "q:power"].into_iter().enumerate() {
        let (r, c) = ((i / 2) as i32, (i % 2) as i32);
        v.push(Item::new(id, Rect { x: x + c * (tw + 14), y: y + r * (104 + 14), w: tw, h: 104 }, Area::Panel));
    }
    // Above the closing hint at the panel's foot (24 px and 16 px between).
    let bottom = bh - 40 - 24 - 16 - 60;
    v.push(Item::new("q:desktop", Rect { x, y: bottom - 72, w, h: 60 }, Area::Panel));
    let hw = (w - 12) / 2;
    v.push(Item::new("q:reboot", Rect { x, y: bottom, w: hw, h: 60 }, Area::Panel));
    v.push(Item::new("q:off", Rect { x: x + hw + 12, y: bottom, w: hw, h: 60 }, Area::Panel));
    v
}

/// GM-6c: a question's box ("Wirklich beenden? ...", "Alle Programme werden beendet."): in the quick panel, or in
/// the middle of the screen (Einstellungen › System).
pub fn confirm_box(bw: i32, bh: i32, panel: bool) -> Rect {
    if panel {
        Rect { x: bw - QUICK_W + 48, y: bh / 2 - 150, w: QUICK_W - 96, h: 300 }
    } else {
        Rect { x: (bw - 760) / 2, y: (bh - 300) / 2, w: 760, h: 300 }
    }
}

/// The question's Ja and Nein (Nein first in the focus: the safe side).
pub fn confirm(bw: i32, bh: i32, panel: bool) -> Vec<Item> {
    let b = confirm_box(bw, bh, panel);
    let w = (b.w - 2 * 28 - 12) / 2;
    let y = b.y + b.h - 28 - 60;
    vec![Item::new("c:yes", Rect { x: b.x + 28, y, w, h: 60 }, Area::Panel), Item::new("c:no", Rect { x: b.x + 28 + w + 12, y, w, h: 60 }, Area::Panel)]
}

/// GM-6d: the on-screen keyboard's level (§2.6): the letters, or `&123`'s symbols and digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Level {
    #[default]
    Letters,
    Symbols,
}

/// The keyboard's rows (German layout, §2.6, board Tastatur): the label and the width of each key; `enter` is the
/// last key's word (Suchen, or Fertig on a settings field). The symbols level keeps the digit row and holds the
/// 24 symbols of §2.6; its `abc` goes back to the letters.
pub fn key_rows(level: Level, enter: &'static str) -> Vec<Vec<(&'static str, i32)>> {
    let keys = |s: &[&'static str]| s.iter().map(|k| (*k, 88)).collect::<Vec<_>>();
    let digits = {
        let mut v = keys(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "ß"]);
        v.push(("⌫ Löschen", 180));
        v
    };
    let mut rows = vec![digits];
    match level {
        Level::Letters => {
            rows.push(keys(&["q", "w", "e", "r", "t", "z", "u", "i", "o", "p", "ü"]));
            rows.push(keys(&["a", "s", "d", "f", "g", "h", "j", "k", "l", "ö", "ä"]));
            let mut r = vec![("⇧ Groß", 140)];
            r.extend(keys(&["y", "x", "c", "v", "b", "n", "m", "-", "."]));
            rows.push(r);
        }
        Level::Symbols => {
            rows.push(keys(&["!", "\"", "§", "$", "%", "&", "/", "(", ")", "=", "?"]));
            rows.push(keys(&["@", "€", "#", "+", "*", "'", "_", ":", ";", "<", ">"]));
            rows.push(keys(&[",", "-", "."]));
        }
    }
    rows.push(vec![(if level == Level::Letters { "&123" } else { "abc" }, 140), ("Leerzeichen", 760), (enter, 220)]);
    rows
}

/// The search screen's places (§2.6, board Tastatur; padding 64 top, 160 at the sides).
pub const SEARCH_SIDE: i32 = 160;
pub const FIELD: Rect = Rect { x: SEARCH_SIDE, y: 64, w: 1920 - 2 * SEARCH_SIDE, h: 96 };
pub const HIT_Y: i32 = 64 + 96 + 32;
pub const HIT_H: i32 = 120;
pub const HIT_GAP: i32 = 24;
/// The hint line's height under the keys (Figtree 17 on a 26 px line, 6 px above it).
pub const KEY_HINT_H: i32 = 26;

/// The keyboard panel's top: padding 36 above the rows, 14 between them and the hint line, 44 below it.
pub fn keys_top(bh: i32) -> i32 {
    bh - 44 - KEY_HINT_H - 6 - 14 - (5 * 76 + 4 * 14) - 36
}

/// The hit cards' width (three in a row between the sides).
pub fn hit_w(bw: i32) -> i32 {
    (bw - 2 * SEARCH_SIDE - 2 * HIT_GAP) / 3
}

/// The search screen's focus elements: with a text, the three hit cards (`hit:0` the best program, `hit:1`
/// YouTube, `hit:2` Brave) and the further program matches `hit:3..` in a second row (`more` of them, shown from
/// `first`: the ones before it lie left of the screen, so the focus walks on and the row follows it); then the
/// keys, each row centred (`key:<label>`, the second level's labels on it).
pub fn search(bw: i32, bh: i32, hits: bool, more: usize, first: usize, level: Level, enter: &'static str) -> Vec<Item> {
    let mut v = Vec::new();
    let hw = hit_w(bw);
    if hits {
        for i in 0..3 {
            v.push(Item::new(format!("hit:{i}"), Rect { x: SEARCH_SIDE + i * (hw + HIT_GAP), y: HIT_Y, w: hw, h: HIT_H }, Area::Keys));
        }
        for i in 0..more {
            let x = SEARCH_SIDE + (i as i32 - first as i32) * (hw + HIT_GAP);
            v.push(Item::new(format!("hit:{}", i + 3), Rect { x, y: HIT_Y + HIT_H + HIT_GAP, w: hw, h: HIT_H }, Area::Keys));
        }
    }
    let top = keys_top(bh) + 36;
    for (r, row) in key_rows(level, enter).iter().enumerate() {
        let width: i32 = row.iter().map(|(_, w)| w).sum::<i32>() + 12 * (row.len() as i32 - 1);
        let mut x = (bw - width) / 2;
        for (label, w) in row {
            v.push(Item::new(format!("key:{label}"), Rect { x, y: top + r as i32 * 90, w: *w, h: 76 }, Area::Keys));
            x += w + 12;
        }
    }
    v
}
