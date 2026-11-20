//! The settings page's layout in logical pixels with its hit boxes (pure). A column grows to `MAX_W` (a line
//! of label and control stays readable); a window wide enough for two such columns of at least `COL_MIN` lays
//! the sections out in two (the first ones down the left, the rest down the right, split where both are about
//! as long), so a page with several sections uses a wide screen instead of leaving half of it empty.

use crate::layout::Rect;
use crate::page::{
    CTL_H, FAV_H, HEAD_AREA, HEAD_H, Heads, Inputs, Kind, Layout, MAX_W, Model, NARROW, Part, RESET_W, ROW_H, Section, UNDO_W,
    control_place_beside, controls, controls_width,
};

/// The favourites' head above their card, and as the card's title on the settings tabs.
pub const FAV_HEAD: &str = "FAVORITEN";
pub const FAV_TITLE: &str = "Favoriten";

/// Two columns only when each gets at least this.
pub const COL_MIN: i32 = 680;
const GAP: i32 = 32;

/// What laying out sections collects.
#[derive(Default)]
struct Acc {
    parts: Vec<(Part, Rect)>,
    heads: Vec<(String, Rect)>,
    cards: Vec<Rect>,
    rows: Vec<Rect>,
    last: Vec<bool>,
    extra: Vec<i32>,
    favs: Vec<Rect>,
    fav_add_row: Rect,
}

/// WY-SET: a head inside its card (the settings tabs): its height, and the room above and below it.
fn inside(h: Heads) -> Option<i32> {
    match h {
        Heads::Above => None,
        Heads::Title => Some(28),
        Heads::Crumb => Some(16),
    }
}

/// Lays section `s` out from `y` in the column at `x`, `w` wide; returns where the next one starts.
fn section(a: &mut Acc, s: &Section, m: &Model, i: &Inputs, x: i32, mut y: i32, w: i32, narrow: bool) -> i32 {
    match s {
        Section::Rows(name, range) => {
            let range = range.start.min(m.rows.len())..range.end.min(m.rows.len());
            if range.is_empty() {
                return y;
            }
            let top = match inside(m.heads) {
                None => {
                    a.heads.push((name.clone(), Rect { x, y, w, h: HEAD_H }));
                    y += HEAD_H + 10;
                    y
                }
                Some(hh) => {
                    let top = y;
                    if !name.is_empty() {
                        a.heads.push((name.clone(), Rect { x: x + 22, y: y + 18, w: w - 44, h: hh }));
                        y += 18 + hh + 4;
                    }
                    top
                }
            };
            for r in range.clone() {
                let row = &m.rows[r];
                let reset = m.flags.get(r).is_some_and(|f| f.reset);
                let extra = if reset { RESET_W + 12 } else { 0 };
                let (cx, cy, avail, h) = control_place_beside(row, x, y, w, narrow, extra);
                a.parts.extend(controls(row, r, cx, cy, avail));
                if let Kind::Fold { .. } = row.kind {
                    a.parts.push((Part::Fold(r), Rect { x, y, w, h }));
                }
                if reset {
                    let at = if h == ROW_H { cx - RESET_W - 12 } else { cx + controls_width(row, avail) + 12 };
                    a.parts.push((Part::Reset(r), Rect { x: at, y: cy, w: RESET_W, h: CTL_H }));
                }
                a.rows[r] = Rect { x, y, w, h };
                a.last[r] = r + 1 == range.end;
                a.extra[r] = extra;
                y += h;
            }
            if inside(m.heads).is_some() {
                y += 6;
            }
            a.cards.push(Rect { x, y: top, w, h: y - top });
            y + if inside(m.heads).is_some() { 18 } else { 28 }
        }
        Section::Favourites => {
            let top = match inside(m.heads) {
                None => {
                    a.heads.push((FAV_HEAD.into(), Rect { x, y, w, h: HEAD_H }));
                    y += HEAD_H + 10;
                    y
                }
                Some(hh) => {
                    let top = y;
                    a.heads.push((FAV_TITLE.into(), Rect { x: x + 22, y: y + 18, w: w - 44, h: hh }));
                    y += 18 + hh + 4;
                    top
                }
            };
            for f in 0..i.favourites {
                let bx = x + w - 16 - 32;
                a.parts.push((Part::FavRemove(f), Rect { x: bx, y: y + 8, w: 32, h: 32 }));
                if f > 0 {
                    a.parts.push((Part::FavUp(f), Rect { x: bx - 36, y: y + 8, w: 32, h: 32 }));
                }
                a.favs.push(Rect { x, y, w, h: FAV_H });
                y += FAV_H;
            }
            a.fav_add_row = Rect { x, y, w, h: FAV_H + 12 };
            let add_w = 130;
            a.parts.push((Part::FavField, Rect { x: x + 16, y: y + 12, w: w - 32 - add_w - 8, h: CTL_H }));
            a.parts.push((Part::FavAdd, Rect { x: x + w - 16 - add_w, y: y + 12, w: add_w, h: CTL_H }));
            y += FAV_H + 12 + 12;
            a.cards.push(Rect { x, y: top, w, h: y - top });
            y + 28
        }
    }
}

/// Where the right column begins: the first section of it (the sections before it go left), so that the
/// longer column is as short as it can be.
fn split(heights: &[i32]) -> usize {
    let total: i32 = heights.iter().sum();
    let mut left = 0;
    let mut best = (i32::MAX, heights.len());
    for k in 1..heights.len() {
        left += heights[k - 1];
        if left.max(total - left) < best.0 {
            best = (left.max(total - left), k);
        }
    }
    best.1
}

/// The page of model `m` in content rectangle `c`, scrolled by `i.scroll` logical pixels.
pub fn layout(c: Rect, m: &Model, i: &Inputs) -> Layout {
    let pad = (c.w / 24).clamp(16, 40);
    let room = c.w - 2 * pad;
    let shown = m.sections.iter().filter(|s| !matches!(s, Section::Rows(_, r) if r.start.min(m.rows.len()) >= r.end.min(m.rows.len()))).count();
    let col_min = if m.col_min > 0 { m.col_min } else { COL_MIN };
    let two = shown >= 2 && room >= 2 * col_min + GAP;
    let col = if two { ((room - GAP) / 2).min(MAX_W) } else { room.clamp(200, MAX_W) };
    let w = if two { 2 * col + GAP } else { col };
    let x = c.x + pad;
    let narrow = col < NARROW;
    // The head (title, address, answer line, Rückgängig) stays; the rest scrolls below it.
    let y = c.y + 24;
    let title = Rect { x, y, w: w - UNDO_W - 12, h: 40 };
    let undo = Rect { x: x + w - UNDO_W, y: y + 2, w: UNDO_W, h: CTL_H };
    let sub = Rect { x, y: y + 46, w, h: 18 };
    let msg = Rect { x, y: y + 70, w, h: 20 };
    let head = Rect { x: c.x, y: c.y, w: c.w, h: HEAD_AREA };
    let start = c.y + HEAD_AREA + 16 - i.scroll;
    let fresh = || Acc { rows: vec![Rect::default(); m.rows.len()], last: vec![false; m.rows.len()], extra: vec![0; m.rows.len()], ..Acc::default() };
    let right_from = if two {
        let heights: Vec<i32> = m.sections.iter().map(|s| section(&mut fresh(), s, m, i, 0, 0, col, narrow)).collect();
        split(&heights)
    } else {
        m.sections.len()
    };
    let mut a = fresh();
    let (mut left, mut right) = (start, start);
    for (k, s) in m.sections.iter().enumerate() {
        if k < right_from {
            left = section(&mut a, s, m, i, x, left, col, narrow);
        } else {
            right = section(&mut a, s, m, i, x + col + GAP, right, col, narrow);
        }
    }
    let end = left.max(right);
    // Only what can be seen can be hit.
    let below = c.y + HEAD_AREA;
    a.parts.retain(|(_, b)| b.y >= below && b.y + b.h <= c.y + c.h);
    if undo.y + undo.h <= c.y + c.h {
        a.parts.insert(0, (Part::Undo, undo));
    }
    let total = end + i.scroll - c.y;
    Layout {
        content: c,
        narrow,
        head,
        scrolled: i.scroll > 0,
        title,
        sub,
        msg,
        heads: a.heads,
        cards: a.cards,
        rows: a.rows,
        last: a.last,
        extra: a.extra,
        favs: a.favs,
        fav_add_row: a.fav_add_row,
        parts: a.parts,
        total,
        heads_look: m.heads,
        rail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Kind, Row};

    fn model(sections: usize, rows: usize) -> Model {
        let row = |k: usize| Row {
            key: format!("k.{k}"),
            kind: Kind::Switch,
            default: "false".into(),
            label: format!("Zeile {k}"),
            short: String::new(),
            note: String::new(),
            confirm: false,
            stack: false,
        };
        let mut m = Model::default();
        for s in 0..sections {
            let from = m.rows.len();
            m.rows.extend((0..rows).map(|r| row(s * rows + r)));
            m.sections.push(Section::Rows(format!("S{s}"), from..m.rows.len()));
        }
        m
    }

    #[test]
    fn a_wide_window_gets_two_columns_and_a_narrow_one_keeps_one() {
        let i = Inputs { favourites: 0, scroll: 0 };
        let wide = Rect { x: 0, y: 0, w: 2400, h: 1300 };
        let l = layout(wide, &model(4, 3), &i);
        let xs: Vec<i32> = l.heads.iter().map(|(_, r)| r.x).collect();
        assert_eq!(xs, [40, 40, 40 + MAX_W + GAP, 40 + MAX_W + GAP], "two left, two right");
        assert_eq!(l.heads[2].1.y, l.heads[0].1.y, "both columns start at the top");
        assert_eq!(l.title.w, 2 * MAX_W + GAP - UNDO_W - 12, "the head spans both columns");
        assert!(l.rows.iter().all(|r| r.w == MAX_W));
        let one = layout(wide, &model(1, 12), &i);
        assert!(one.heads.iter().all(|(_, r)| r.x == 40) && one.rows[0].w == MAX_W, "one section stays one column");
        let narrow = layout(Rect { w: 1300, ..wide }, &model(4, 3), &i);
        assert!(narrow.heads.iter().all(|(_, r)| r.x == narrow.heads[0].1.x), "1300 px: one column, as before");
        assert_eq!(split(&[100, 100, 100, 100]), 2);
        assert_eq!(split(&[300, 50, 50, 50]), 1);
        assert_eq!(split(&[50, 50, 50, 300]), 3);
    }
}
