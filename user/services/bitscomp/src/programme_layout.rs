//! PK-7a: the "Programme" page's geometry in logical pixels (pure: host tests), from the item list of
//! `programme_view`. A fixed head (title, address, answer line, the tabs, the link to the sources) and a body that
//! scrolls under it: section heads, panel cards of lines, rows, buttons and boxes, the bare search field and the
//! library's tile rows. Every control is a hit part; only what is wholly in view can be hit.

use crate::layout::Rect;
use crate::page::Part;
use crate::programme_view::{Item, ProgPart};

pub const HEAD_AREA: i32 = 180;
pub const LINE_H: i32 = 24;
pub const ROW_H: i32 = 72;
pub const BUTTONS_H: i32 = 52;
pub const CHECK_H: i32 = 36;
pub const PROGRESS_H: i32 = 52;
pub const SEARCH_H: i32 = 48;
pub const TILE_W: i32 = 128;
pub const TILE_H: i32 = 140;
pub const TILE_GAP: i32 = 12;
pub const MORE: i32 = 28;
pub const CTL_H: i32 = 36;
/// Wide enough for ten tiles a row on a 2560 px screen; the lines of a row stay readable at this width.
const MAX_W: i32 = 1440;
const SOURCES_W: i32 = 190;
const TAB_W: i32 = 180;

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub content: Rect,
    pub head: Rect,
    pub scrolled: bool,
    pub title: Rect,
    pub sub: Rect,
    pub msg: Rect,
    /// One per item (a head's label, a line's band, a row, the field, a tile row's title).
    pub items: Vec<Rect>,
    /// Panel cards behind runs of card items.
    pub cards: Vec<Rect>,
    /// Every tile by its page-wide number.
    pub tiles: Vec<Rect>,
    pub parts: Vec<(Part, Rect)>,
    pub total: i32,
}

/// A button's width for its label (the layout knows no font: about 8 px a character of the 14 px control font).
pub fn button_w(label: &str) -> i32 {
    (label.chars().count() as i32 * 8 + 36).max(112)
}

/// Buttons in a row of width `w`, wrapped onto more lines where they do not fit: per line (index, x, width).
fn button_lines<'a>(labels: impl Iterator<Item = &'a str>, w: i32) -> Vec<Vec<(usize, i32, i32)>> {
    let mut lines: Vec<Vec<(usize, i32, i32)>> = Vec::new();
    let mut x = 0;
    for (k, l) in labels.enumerate() {
        let bw = button_w(l);
        if lines.is_empty() || (x > 0 && x + bw > w) {
            lines.push(Vec::new());
            x = 0;
        }
        if let Some(line) = lines.last_mut() {
            line.push((k, x, bw));
        }
        x += bw + 8;
    }
    lines
}

fn tile_cols(w: i32) -> usize {
    ((w + TILE_GAP) / (TILE_W + TILE_GAP)).max(1) as usize
}

pub fn layout(c: Rect, items: &[Item], tabs: &[String], scroll: i32) -> Layout {
    let pad = (c.w / 24).clamp(16, 40);
    let w = (c.w - 2 * pad).clamp(200, MAX_W);
    let x = c.x + pad;
    let top = c.y + 24;
    let title = Rect { x, y: top, w: (w - SOURCES_W - 12).max(1), h: 40 };
    let sources = Rect { x: x + w - SOURCES_W, y: top + 2, w: SOURCES_W, h: CTL_H };
    let sub = Rect { x, y: top + 46, w, h: 18 };
    let msg = Rect { x, y: top + 70, w, h: 20 };
    let tab_w = TAB_W.min((w - 8 * tabs.len().saturating_sub(1) as i32) / tabs.len().max(1) as i32).max(40);
    let mut head_parts: Vec<(Part, Rect)> = (0..tabs.len()).map(|i| (Part::Prog(ProgPart::Tab(i)), Rect { x: x + i as i32 * (tab_w + 8), y: top + 100, w: tab_w, h: CTL_H })).collect();
    head_parts.push((Part::Prog(ProgPart::Sources), sources));
    let head = Rect { x: c.x, y: c.y, w: c.w, h: HEAD_AREA };
    let mut parts: Vec<(Part, Rect)> = Vec::new();
    let mut rects = Vec::with_capacity(items.len());
    let mut cards = Vec::new();
    let mut tiles = Vec::new();
    let mut y = c.y + HEAD_AREA + 16 - scroll;
    let mut card: Option<i32> = None;
    let close = |card: &mut Option<i32>, y: &mut i32, cards: &mut Vec<Rect>| {
        if let Some(t) = card.take() {
            *y += 8;
            cards.push(Rect { x, y: t, w, h: *y - t });
            *y += 28;
        }
    };
    let ix = x + 20;
    let iw = (w - 40).max(1);
    for it in items {
        match it {
            Item::Head(_) => {
                close(&mut card, &mut y, &mut cards);
                rects.push(Rect { x, y, w, h: 18 });
                y += 28;
            }
            Item::Search => {
                close(&mut card, &mut y, &mut cards);
                let r = Rect { x, y, w, h: SEARCH_H };
                parts.push((Part::Prog(ProgPart::Search), r));
                rects.push(r);
                y += SEARCH_H + 28;
            }
            Item::Tiles(_, list, first) => {
                close(&mut card, &mut y, &mut cards);
                rects.push(Rect { x, y, w, h: 18 });
                y += 28;
                let cols = tile_cols(w);
                for (i, t) in list.iter().enumerate() {
                    let (row, col) = ((i / cols) as i32, (i % cols) as i32);
                    let r = Rect { x: x + col * (TILE_W + TILE_GAP), y: y + row * (TILE_H + TILE_GAP), w: TILE_W, h: TILE_H };
                    if t.package.is_some() {
                        parts.push((Part::Prog(ProgPart::More(first + i)), Rect { x: r.x + r.w - MORE - 4, y: r.y + 4, w: MORE, h: MORE }));
                    }
                    parts.push((Part::Prog(ProgPart::Tile(first + i)), r));
                    tiles.push(r);
                }
                let lines = list.len().div_ceil(cols) as i32;
                y += lines * (TILE_H + TILE_GAP) + 16;
            }
            _ => {
                if card.is_none() {
                    card = Some(y);
                    y += 8;
                }
                let h = match it {
                    Item::Line(..) => LINE_H,
                    Item::Row { .. } => ROW_H,
                    Item::Buttons(list) => BUTTONS_H * button_lines(list.iter().map(|b| b.1.as_str()), iw).len().max(1) as i32,
                    Item::Check(..) => CHECK_H,
                    Item::Progress(..) => PROGRESS_H,
                    _ => 0,
                };
                let r = Rect { x: ix, y, w: iw, h };
                match it {
                    Item::Row { buttons, .. } => {
                        let mut bx = ix + iw;
                        for (p, label) in buttons.iter().rev() {
                            let bw = button_w(label);
                            bx -= bw;
                            parts.push((Part::Prog(*p), Rect { x: bx, y: y + (ROW_H - CTL_H) / 2, w: bw, h: CTL_H }));
                            bx -= 8;
                        }
                    }
                    Item::Buttons(list) => {
                        for (line, at) in button_lines(list.iter().map(|b| b.1.as_str()), iw).iter().enumerate() {
                            for &(k, bx, bw) in at {
                                let by = y + line as i32 * BUTTONS_H + (BUTTONS_H - CTL_H) / 2;
                                parts.push((Part::Prog(list[k].0), Rect { x: ix + bx, y: by, w: bw.min(iw), h: CTL_H }));
                            }
                        }
                    }
                    Item::Check(p, ..) => parts.push((Part::Prog(*p), r)),
                    _ => {}
                }
                rects.push(r);
                y += h;
            }
        }
    }
    close(&mut card, &mut y, &mut cards);
    let below = c.y + HEAD_AREA;
    parts.retain(|(_, b)| b.y >= below && b.y + b.h <= c.y + c.h);
    head_parts.retain(|(_, b)| b.y + b.h <= c.y + c.h);
    head_parts.extend(parts);
    Layout { content: c, head, scrolled: scroll > 0, title, sub, msg, items: rects, cards, tiles, parts: head_parts, total: y + scroll - c.y }
}

impl Layout {
    pub fn hit(&self, x: f64, y: f64) -> Option<Part> {
        crate::page_kind::hit(self.content, &self.parts, x, y)
    }

    pub fn part(&self, p: ProgPart) -> Option<Rect> {
        self.parts.iter().find(|(q, _)| *q == Part::Prog(p)).map(|(_, b)| *b)
    }

    pub fn max_scroll(&self) -> i32 {
        crate::page_kind::max_scroll(self.total, self.content.h)
    }
}
