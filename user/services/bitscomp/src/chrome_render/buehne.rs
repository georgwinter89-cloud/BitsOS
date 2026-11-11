//! DS-3: the Bühne preset's geometry (theme.layout.preset = buehne, the default; Main.dc.html of the design v2,
//! plan/spec/desktop.md §0 answers 3, 5, 8). Board px (1920x1080) laid onto the output: at 2560x1440 and 125 %
//! (2048x1152 logical) every rect is the board's times 4/3 (`Theme::bl`); other sizes keep the margins and let the
//! stage take what is left. The header (mark, workspace chips, the context address, status blocks, Claude's ring,
//! sun, bell, clock), the stage (the tiles' place, with the optional Absicht headline), the right column (map and
//! "Als Nächstes" frames), the time strip and the bottom row (hint line, Feld bar, minimised windows, Startseite).
//! `chrome` projects it onto WY's `Chrome`, so hit tests, `ctl hot`, the tiles' area and the address editing work
//! unchanged; `hit` finds the parts WY's `Part` has no name for (sun, bell, Feld bar, strip).

use crate::chrome::{Chrome, logical_width};
use crate::layout::Rect;
use crate::text::{Family, Weight};
use crate::theme::Theme;

/// What the layout depends on besides the theme (all logical).
#[derive(Clone, Debug, PartialEq)]
pub struct Inputs {
    pub size: (i32, i32),
    pub names: Vec<String>,
    /// The shown workspace (its chip is lit and always shown).
    pub active: usize,
    pub min_labels: Vec<String>,
    /// The shown workspace has an Absicht headline (`desktop.absicht.<slug>`).
    pub absicht: bool,
}

/// The parts of the Bühne that WY's `chrome::Part` does not name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The light/dark toggle (theme.mode).
    Sun,
    /// The notices (count 0 until there are notices).
    Bell,
    /// The Feld bar (the Feld itself opens in a later item).
    Feld,
    /// The time strip (filled by the snapshots later).
    Strip,
    /// The "+N" chip of workspaces that do not fit.
    More,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Buehne {
    /// The output (logical).
    pub size: (i32, i32),
    pub header: Rect,
    pub mark: Rect,
    /// One per workspace in order; only the first `shown` are on screen.
    pub chips: Vec<Rect>,
    pub shown: usize,
    /// The "+N" chip when not every chip fits.
    pub more: Option<Rect>,
    pub ws_new: Rect,
    /// PK-L1b: the "Programme" button left of the address.
    pub programs: Rect,
    /// The read-only context address.
    pub address: Rect,
    /// Where the address suggestions hang (Super+L): under the address, at least SUGGEST_MIN wide, no height.
    pub suggest: Rect,
    pub blocks: Rect,
    pub ring: Rect,
    pub sun: Rect,
    pub bell: Rect,
    pub clock: Rect,
    pub stage: Rect,
    pub headline: Option<Rect>,
    /// The tiles' area (inside the stage, below the headline).
    pub work: Rect,
    /// The right column's two frames (map, "Als Nächstes"); none when folded or when the output is too narrow.
    pub column: Option<(Rect, Rect)>,
    pub strip: Option<Rect>,
    /// The bottom row: hint line, Feld bar, minimised windows and Startseite.
    pub bottom: Option<Rect>,
    pub hint: Rect,
    pub feld: Rect,
    pub home: Option<Rect>,
    pub min_items: Vec<Rect>,
}

/// Board px of the parts (Main.dc.html).
const MARGIN: f32 = 48.0;
const HEADER: f32 = 56.0;
const PAD: f32 = 32.0;
const CHIP_H: f32 = 36.0;
const CHIP_GAP: f32 = 8.0;
const BUTTON: f32 = 40.0;
const BLOCKS: f32 = 420.0;
const CLOCK: f32 = 170.0;
const COLUMN: f32 = 432.0;
const MAP_H: f32 = 316.0;
/// "Als Nächstes" as on the board (416..668); 688..800 stays free for the hung-program card of a later item.
const NEXT_H: f32 = 252.0;
const STRIP_H: f32 = 128.0;
const ROW_H: f32 = 64.0;
const FELD_W: f32 = 960.0;
const HINT_W: f32 = 400.0;
/// The headline's place on the stage: label, 12 px, the 48 px line, then the tiles' gap.
const HEAD_TOP: f32 = 28.0;
const HEAD_H: f32 = 76.0;
/// Narrower than this the stage keeps the column's room (the column goes).
const STAGE_MIN: f32 = 800.0;
/// Lower than this (with the strip) the stage keeps the strip's room: the strip goes.
const STAGE_MIN_H: f32 = 480.0;
/// The context address keeps at least this much room; chips beyond it fold into "+N".
const ADDRESS_MIN: f32 = 240.0;
/// The suggestions under the address (Super+L) are at least this wide.
const SUGGEST_MIN: f32 = 560.0;

/// A rect that is nowhere (zero size, below everything): parts WY's chrome has but the Bühne has not.
pub fn gone(h: i32) -> Rect {
    Rect { x: 0, y: h.max(0) + 1, w: 0, h: 0 }
}

/// A chip's text style and its width: 14 px padding, the 8 px dot and its 8 px gap, the name.
pub fn chip_w(t: &Theme, name: &str) -> i32 {
    let f = t.bfont(Family::Ui, 13.0, Weight::SemiBold);
    logical_width(&f, name, t.scale) + t.bl(44.0)
}

/// The text of the "+N" chip.
pub fn more_text(n: usize) -> String {
    format!("+{n}")
}

fn more_w(t: &Theme, n: usize) -> i32 {
    let f = t.bfont(Family::Mono, 12.0, Weight::Medium);
    logical_width(&f, &more_text(n), t.scale) + t.bl(24.0)
}

pub fn layout(t: &Theme, i: &Inputs) -> Buehne {
    let (w, h) = (i.size.0.max(1), i.size.1.max(1));
    let b = |px: f32| t.bl(px);
    let header = Rect { x: 0, y: 0, w, h: b(HEADER) };
    let mid = |hh: i32| (header.h - hh) / 2;
    // Right to left: clock, bell, sun, Claude's ring, status blocks (fixed slots, so a changing value never moves
    // the chips).
    let right = w - b(PAD);
    let clock = Rect { x: right - b(CLOCK), y: mid(b(44.0)), w: b(CLOCK), h: b(44.0) };
    let button = |x: i32| Rect { x, y: mid(b(BUTTON)), w: b(BUTTON), h: b(BUTTON) };
    let bell = button(clock.x - b(45.0) - b(BUTTON));
    let sun = button(bell.x - b(14.0) - b(BUTTON));
    let ring = button(sun.x - b(14.0) - b(BUTTON));
    let mark_w = {
        let f = t.bfont(Family::Display, 17.0, Weight::SemiBold);
        b(36.0) + logical_width(&f, "BitsOS", t.scale)
    };
    let mark = Rect { x: b(PAD), y: mid(b(44.0)), w: mark_w, h: b(44.0) };
    let chips_x = mark.x + mark.w + b(49.0);
    let bw = b(BLOCKS).min((ring.x - b(22.0) - chips_x - b(400.0)).max(0));
    let blocks = Rect { x: ring.x - b(22.0) - bw, y: mid(b(36.0)), w: bw, h: b(36.0) };
    // Chips from the left; what does not fit goes into "+N" (lit when the shown workspace is among them; those are
    // reached by Ctrl+Alt+digit and `ctl ws`).
    let limit = blocks.x - b(24.0) - b(ADDRESS_MIN);
    let widths: Vec<i32> = i.names.iter().map(|n| chip_w(t, n)).collect();
    let new_w = b(CHIP_H);
    let fits = |n: usize| {
        let more = if n < widths.len() { more_w(t, widths.len() - n) + b(CHIP_GAP) } else { 0 };
        chips_x + widths[..n].iter().map(|x| x + b(CHIP_GAP)).sum::<i32>() + more + new_w <= limit
    };
    let mut shown = widths.len();
    while shown > 0 && !fits(shown) {
        shown -= 1;
    }
    let cy = mid(b(CHIP_H));
    let mut x = chips_x;
    let mut chips = Vec::with_capacity(widths.len());
    for (k, cw) in widths.iter().enumerate() {
        if k < shown {
            chips.push(Rect { x, y: cy, w: *cw, h: b(CHIP_H) });
            x += cw + b(CHIP_GAP);
        } else {
            chips.push(Rect { y: i32::MAX / 4, ..gone(h) });
        }
    }
    let more = (shown < widths.len()).then(|| {
        let r = Rect { x, y: cy, w: more_w(t, widths.len() - shown), h: b(CHIP_H) };
        x += r.w + b(CHIP_GAP);
        r
    });
    let ws_new = Rect { x, y: cy, w: new_w, h: b(CHIP_H) };
    let programs = Rect { x: ws_new.x + ws_new.w + b(24.0), y: mid(b(36.0)), w: b(36.0), h: b(36.0) };
    let ax = programs.x + programs.w + b(12.0);
    let address = Rect { x: ax, y: mid(b(36.0)), w: (blocks.x - b(24.0) - ax).max(1), h: b(36.0) };
    let sw = address.w.max(b(SUGGEST_MIN)).min(w);
    let suggest = Rect { x: address.x.min(w - sw).max(0), y: header.h, w: sw, h: 0 };
    // The bottom: the time strip and the row under it (theme.layout.statusbar off: neither).
    let (strip, bottom) = if t.layout.statusbar {
        // Measured from the board's bottom edge as sums, so 2560x1440 lands on the board's own pixels.
        let row = Rect { x: b(MARGIN), y: h - b(100.0), w: (w - 2 * b(MARGIN)).max(1), h: b(ROW_H) };
        let strip = Rect { x: b(MARGIN), y: h - b(100.0 + 28.0 + STRIP_H), w: row.w, h: b(STRIP_H) };
        // A low output keeps the stage's room for the tiles: the empty strip goes first.
        let room = strip.y - b(24.0) - b(HEADER + 24.0);
        (if room >= b(STAGE_MIN_H) { Some(strip) } else { None }, Some(row))
    } else {
        (None, None)
    };
    let stage_y = b(HEADER + 24.0);
    let stage_bottom = match (strip, bottom) {
        (Some(_), _) => h - b(100.0 + 28.0 + STRIP_H + 24.0),
        (None, Some(_)) => h - b(100.0 + 24.0),
        (None, None) => h - b(36.0),
    }
    .max(stage_y + 1);
    let col_x = w - b(MARGIN + COLUMN);
    let with_column = t.layout.sidebar && col_x - b(PAD) - b(MARGIN) >= b(STAGE_MIN);
    let stage_right = if with_column { w - b(MARGIN + COLUMN + PAD) } else { w - b(MARGIN) };
    let stage = Rect { x: b(MARGIN), y: stage_y, w: (stage_right - b(MARGIN)).max(1), h: stage_bottom - stage_y };
    let column = with_column.then(|| {
        let map = Rect { x: col_x, y: stage.y, w: b(COLUMN), h: b(MAP_H).min(stage.h / 2) };
        let ny = b(HEADER + 24.0 + MAP_H + 20.0).max(map.y + map.h + 1);
        (map, Rect { x: col_x, y: ny, w: b(COLUMN), h: b(NEXT_H).min(stage_bottom - ny).max(1) })
    });
    let headline = (i.absicht && stage.h > b(HEAD_TOP + HEAD_H) * 2)
        .then(|| Rect { x: stage.x + b(PAD), y: stage.y + b(HEAD_TOP), w: (stage.w - 2 * b(PAD)).max(1), h: b(HEAD_H) });
    let work = match headline {
        Some(hl) => Rect { y: hl.y + hl.h, h: stage.y + stage.h - hl.y - hl.h, ..stage },
        None => stage,
    };
    // The bottom row: hint at the left, the Feld bar in the middle, minimised windows and Startseite at the right.
    let row = bottom.unwrap_or(gone(h));
    let side_w = b(HINT_W).min((row.w - b(200.0)) / 2 - b(32.0)).max(0);
    let hint = Rect { x: row.x, y: row.y, w: side_w, h: row.h };
    let fw = b(FELD_W).min(row.w - 2 * side_w - 2 * b(32.0)).max(1);
    let feld = Rect { x: (w - fw) / 2, y: row.y, w: fw, h: row.h };
    let zone = Rect { x: row.x + row.w - side_w, y: row.y + b(10.0), w: side_w, h: b(44.0) };
    let home_w = {
        let f = t.bfont(Family::Ui, 13.0, Weight::SemiBold);
        logical_width(&f, "Startseite", t.scale) + b(28.0)
    };
    let home = (bottom.is_some() && home_w <= zone.w).then(|| Rect { x: zone.x + zone.w - home_w, w: home_w, ..zone });
    let mut min_items = Vec::new();
    if let Some(hr) = home {
        let mut x = hr.x - b(10.0);
        let f = t.bfont(Family::Mono, 12.0, Weight::Medium);
        for l in &i.min_labels {
            let iw = (logical_width(&f, l, t.scale) + b(24.0)).min(b(180.0));
            if x - iw < zone.x {
                break;
            }
            x -= iw;
            min_items.push(Rect { x, y: zone.y + b(4.0), w: iw, h: b(CHIP_H) });
            x -= b(CHIP_GAP);
        }
    }
    Buehne {
        size: (w, h),
        header,
        mark,
        chips,
        shown,
        more,
        ws_new,
        programs,
        address,
        suggest,
        blocks,
        ring,
        sun,
        bell,
        clock,
        stage,
        headline,
        work,
        column,
        strip,
        bottom,
        hint,
        feld,
        home,
        min_items,
    }
}

impl Buehne {
    /// WY's chrome as the Bühne shows it: the chips are the workspace entries, the mark-free header is the top
    /// bar, the right column is the docked sidebar, the strip and the bottom row are the status bar. The address
    /// keeps its place for the suggestions under it but has no height, so a click never starts editing there
    /// (the context address is read-only; Super+L still edits, the text is drawn in the header). No bar folds
    /// over the tiles (the list's rect has no rows, so `edge_at` finds no edge).
    pub fn chrome(&self) -> Chrome {
        let h = self.size.1;
        let column = self.column.map(|(m, n)| Rect { h: n.y + n.h - m.y, ..m });
        let status = match (self.strip, self.bottom) {
            (Some(s), Some(b)) => Some(Rect { h: b.y + b.h - s.y, ..s }),
            (None, b) => b,
            (Some(s), None) => Some(s),
        };
        let row_y = self.bottom.map_or(h, |b| b.y);
        let min_title = if self.min_items.is_empty() { gone(h) } else { Rect { x: 0, y: row_y, w: 0, h: 0 } };
        Chrome {
            top: self.header,
            mark: self.mark,
            back: gone(h),
            forward: gone(h),
            programs: self.programs,
            address: self.suggest,
            ring: self.ring,
            favs: None,
            fav_items: Vec::new(),
            list: gone(h),
            list_shown: true,
            list_docked: true,
            ws_fold: gone(h),
            ws_button: None,
            ws_title: gone(h),
            ws_items: self.chips.clone(),
            ws_new: self.ws_new,
            min_title,
            min_items: self.min_items.clone(),
            home: self.home.unwrap_or(gone(h)),
            side: column,
            side_docked: column.is_some(),
            status,
            work: self.work,
        }
    }

    /// The Bühne's own parts under (x, y).
    pub fn hit(&self, x: f64, y: f64) -> Option<Part> {
        let parts = [(Some(self.sun), Part::Sun), (Some(self.bell), Part::Bell), (self.more, Part::More)];
        if let Some((_, p)) = parts.iter().find(|(r, _)| r.is_some_and(|r| r.contains(x, y))) {
            return Some(*p);
        }
        if self.bottom.is_some() && self.feld.contains(x, y) {
            return Some(Part::Feld);
        }
        self.strip.filter(|s| s.contains(x, y)).map(|_| Part::Strip)
    }
}

/// The context address's name of a workspace (`kontext/steuer-2026`) and its settings key part: lower case ASCII,
/// umlauts spelt out, every other run of signs one dash.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().chars().flat_map(char::to_lowercase) {
        let s = match ch {
            'ä' => "ae",
            'ö' => "oe",
            'ü' => "ue",
            'ß' => "ss",
            c if c.is_ascii_alphanumeric() => {
                out.push(c);
                continue;
            }
            _ => "-",
        };
        if s == "-" {
            if !out.is_empty() && !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push_str(s);
        }
    }
    let s = out.trim_end_matches('-').to_string();
    if s.is_empty() { "raum".into() } else { s }
}

/// The settings key of a workspace's Absicht headline among the workspaces `names`: `desktop.absicht.<slug>`, and
/// when another workspace with a different name has the same slug ("Büro" and "Buero"), all of those get a short
/// hash of their exact name added, so no two workspaces share a headline. Workspaces with the very same name do (the
/// user cannot tell them apart either). A collision that begins or ends switches the key (the text stays in the
/// settings under the old one): rare, and it only hides a headline, nothing is lost.
pub fn absicht_key(name: &str, names: &[String]) -> String {
    let (own, exact) = (slug(name), name.trim());
    if names.iter().any(|n| n.trim() != exact && slug(n) == own) {
        let h = exact.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
        format!("desktop.absicht.{own}-{:04x}", h & 0xffff)
    } else {
        format!("desktop.absicht.{own}")
    }
}

/// Which of the three context colours a workspace gets: by its number ("Arbeitsraum 2" -> the second), else by
/// its name, so a workspace keeps its colour when another one goes.
pub fn colour_of(name: &str) -> usize {
    let digits: String = name.chars().rev().take_while(char::is_ascii_digit).collect::<Vec<_>>().into_iter().rev().collect();
    match digits.parse::<usize>() {
        Ok(n) if n > 0 => (n - 1) % 3,
        _ => name.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) as usize % 3,
    }
}

/// The hint line under "Bereit": "2 Teile auf der Bühne · 1 Programm hängt" (no hang part when none hangs).
pub fn hint_text(parts: usize, hung: usize) -> String {
    let p = if parts == 1 { "1 Teil auf der Bühne".to_string() } else { format!("{parts} Teile auf der Bühne") };
    match hung {
        0 => p,
        1 => format!("{p} · 1 Programm hängt"),
        n => format!("{p} · {n} Programme hängen"),
    }
}

#[cfg(test)]
#[path = "buehne_tests.rs"]
mod tests;
