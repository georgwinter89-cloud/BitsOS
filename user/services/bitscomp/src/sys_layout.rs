//! SYS-A: the program "System"'s geometry in logical pixels (pure: host tests). A fixed head (mark, title, address,
//! state, the tab pills, the time range) over a body that scrolls: rows of cards whose items stack top down (a card
//! row is as high as its highest card; a growing row takes what is left), or the task manager's table with its
//! detail card, or the device list. Rows with more cards than fit side by side wrap. Only what lies wholly inside
//! the body can be hit.

use crate::layout::Rect;
use crate::sys_group::{Column, Filter};
use crate::sys_model::{Card, Item, TabModel};
use crate::sys_model3::{Devices, Procs};
use crate::sys_ring::Range;
use crate::sys_rules::Vorrang;
use crate::sys_view::{SysPart, Tab};

pub const PAD: i32 = 24;
pub const GAP: i32 = 20;
pub const ITEM_GAP: i32 = 12;
pub const TAB_H: i32 = 38;
pub const ROW_H: i32 = 40;
pub const GROUP_H: i32 = 34;
pub const HEAD_ROW_H: i32 = 30;
pub const DETAIL_W: i32 = 400;
pub const DEV_LIST_W: i32 = 380;
pub const FACTS_W: i32 = 360;
const CARD_MIN_W: i32 = 190;

/// A text's width estimate for the layout (the drawing measures; 14 px text is about 7.6 px a character).
pub fn text_w(s: &str, px: f32) -> i32 {
    (s.chars().count() as f32 * px * 0.54).ceil() as i32
}

pub fn pill_w(s: &str) -> i32 {
    text_w(s, 14.0) + 34
}

/// Lines a muted 13 px text takes in `w`.
pub fn sub_lines(s: &str, w: i32) -> i32 {
    ((text_w(s, 13.0) as f32 / w.max(1) as f32).ceil() as i32).clamp(1, 3)
}

pub fn item_h(it: &Item, w: i32) -> i32 {
    match it {
        Item::Label(_) | Item::LabelRight(..) | Item::Legend(_) => 16,
        Item::Big(..) => 44,
        Item::Mid(_) => 32,
        Item::Sub(s) => 18 * sub_lines(s, w),
        Item::Row(..) | Item::RowTone(..) => 22,
        Item::Link(..) => 20,
        Item::Bar(_) => 10,
        Item::Chart(c) => c.h + if c.axis.is_some() { 20 } else { 0 },
        Item::Threads(t) => ((t.len().max(1) as i32 + 7) / 8) * 92,
        Item::Grid(g) => ((g.len() as i32 + 1) / 2) * 52,
        Item::Buttons(_) => 42,
        Item::Rule => 1,
        Item::Fill => 0,
    }
}

pub fn card_h(c: &Card, w: i32) -> i32 {
    let inner = w - 2 * PAD;
    let n = c.items.iter().filter(|i| !matches!(i, Item::Fill)).count() as i32;
    2 * PAD + c.items.iter().map(|i| item_h(i, inner)).sum::<i32>() + (n - 1).max(0) * ITEM_GAP
}

/// The items of a card in `r`, top down; after a `Fill` the rest sits at the bottom. Also their hit parts.
pub fn place_items(r: Rect, items: &[Item], parts: &mut Vec<(SysPart, Rect)>) -> Vec<Rect> {
    let inner = Rect { x: r.x + PAD, y: r.y + PAD, w: r.w - 2 * PAD, h: r.h - 2 * PAD };
    let mut out = Vec::with_capacity(items.len());
    let split = items.iter().position(|i| matches!(i, Item::Fill));
    let tail: i32 = split.map_or(0, |s| {
        let rest = &items[s + 1..];
        rest.iter().map(|i| item_h(i, inner.w)).sum::<i32>() + rest.len() as i32 * ITEM_GAP
    });
    let mut y = inner.y;
    for (i, it) in items.iter().enumerate() {
        if Some(i) == split {
            y = y.max(inner.y + inner.h - tail);
            out.push(Rect { x: inner.x, y, w: inner.w, h: 0 });
            continue;
        }
        let h = item_h(it, inner.w);
        let b = Rect { x: inner.x, y, w: inner.w, h };
        match it {
            Item::Link(t, p) => parts.push((*p, Rect { w: (text_w(t, 14.0) + 24).min(inner.w), ..b })),
            Item::LabelRight(..) => {}
            Item::Buttons(bs) => {
                let n = bs.len().max(1) as i32;
                let bw = (inner.w - (n - 1) * 10) / n;
                for (k, (_, p, on)) in bs.iter().enumerate() {
                    if *on {
                        parts.push((*p, Rect { x: inner.x + k as i32 * (bw + 10), y, w: bw, h }));
                    }
                }
            }
            _ => {}
        }
        out.push(b);
        y += h + ITEM_GAP;
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineAt {
    Head(usize),
    Line(usize, usize),
    More(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcLay {
    pub table: Rect,
    pub search: Rect,
    pub filters: Vec<(Filter, Rect)>,
    pub heads: Vec<(Column, Rect)>,
    /// The visible lines of the table.
    pub lines: Vec<(LineAt, Rect)>,
    pub rows_area: Rect,
    pub detail: Rect,
    pub d_name: Rect,
    pub d_line: Rect,
    pub d_rows: Vec<Rect>,
    pub d_prio: (Rect, Vec<(Vorrang, Rect)>),
    pub d_who: Rect,
    pub d_buttons: (Rect, Rect),
    pub d_note: Rect,
    pub total: i32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DevLay {
    pub list: Rect,
    /// (group head or device index, rect).
    pub lines: Vec<(Option<usize>, usize, Rect)>,
    pub pci: Option<Rect>,
    pub detail: Rect,
    pub d_items: Vec<Rect>,
    pub facts: Rect,
    pub f_items: Vec<Rect>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub content: Rect,
    pub head: Rect,
    pub mark: Rect,
    pub title: Rect,
    pub addr: Rect,
    pub status: Option<Rect>,
    pub sub: Rect,
    pub msg: Rect,
    pub tabs: Vec<(Tab, Rect)>,
    pub range_box: Option<Rect>,
    pub ranges: Vec<(Range, Rect)>,
    pub body: Rect,
    pub cards: Vec<Rect>,
    pub items: Vec<Vec<Rect>>,
    pub procs: Option<ProcLay>,
    pub devs: Option<DevLay>,
    pub parts: Vec<(SysPart, Rect)>,
    pub total: i32,
}

impl Layout {
    pub fn hit(&self, x: f64, y: f64) -> Option<SysPart> {
        self.parts.iter().find(|(_, r)| r.contains(x, y)).map(|(p, _)| *p)
    }
    #[cfg(test)]
    pub fn part(&self, p: SysPart) -> Option<Rect> {
        self.parts.iter().find(|(q, _)| *q == p).map(|(_, r)| *r)
    }
    pub fn max_scroll(&self) -> i32 {
        (self.total - self.body.h).max(0)
    }
}

fn inside(body: Rect, r: Rect) -> bool {
    r.y >= body.y && r.y + r.h <= body.y + body.h && r.h > 0
}

pub fn layout(c: Rect, m: &TabModel, tab: Tab, status_text: Option<&str>, scroll: i32) -> Layout {
    let pad = (c.w / 36).clamp(16, 40);
    let x = c.x + pad;
    let w = (c.w - 2 * pad).max(200);
    let top = c.y + 24;
    let mark = Rect { x, y: top + 3, w: 34, h: 34 };
    let title = Rect { x: x + 50, y: top, w: 120, h: 40 };
    // Mono: 0.6 em a character.
    let addr = Rect { x: title.x + title.w + 8, y: top + 10, w: "system/datenträger".chars().count() as i32 * 8 + 12, h: 20 };
    let sub_w = (text_w(&m.sub, 13.0) * 5 / 4).min(w / 2);
    let sub = Rect { x: x + w - sub_w, y: top + 10, w: sub_w, h: 20 };
    let status = status_text.map(|s| {
        let sw = text_w(s, 14.0) + 48;
        Rect { x: sub.x - 16 - sw, y: top + 2, w: sw, h: 36 }
    });
    let mut parts = Vec::new();
    let ty = top + 52;
    let mut tabs = Vec::new();
    let (mut tx, mut row_y) = (x, ty);
    for t in Tab::ALL {
        let pw = pill_w(t.name());
        // A narrow window (a half-screen tile) puts the rest of the tabs on a new row: every tab stays clickable.
        if tx > x && tx + pw > x + w {
            (tx, row_y) = (x, row_y + TAB_H + 6);
        }
        let r = Rect { x: tx, y: row_y, w: pw, h: TAB_H };
        tabs.push((t, r));
        parts.push((SysPart::Tab(t), r));
        tx += r.w + 6;
    }
    let ty = row_y;
    let mut ranges = Vec::new();
    let mut range_box = None;
    if tab.has_range() {
        let rw: i32 = Range::ALL.iter().map(|r| text_w(r.name(), 14.0) + 26).sum::<i32>() + 8 + 4;
        // Narrow windows put the range under the tabs.
        let (bx, by) = if tx + 16 + rw <= x + w { (x + w - rw, ty) } else { (x + w - rw, ty + TAB_H + 8) };
        let b = Rect { x: bx, y: by, w: rw, h: TAB_H };
        let mut rx = b.x + 4;
        for r in Range::ALL {
            let pr = Rect { x: rx, y: b.y + 4, w: text_w(r.name(), 14.0) + 26, h: TAB_H - 8 };
            ranges.push((r, pr));
            parts.push((SysPart::Range(r), pr));
            rx += pr.w + 2;
        }
        range_box = Some(b);
    }
    let head_bottom = range_box.map_or(ty + TAB_H, |b| b.y + b.h).max(ty + TAB_H);
    let msg = Rect { x, y: head_bottom + 4, w, h: 18 };
    let body = Rect { x: c.x, y: head_bottom + 24, w: c.w, h: (c.y + c.h - head_bottom - 24 - 12).max(0) };
    let head = Rect { x: c.x, y: c.y, w: c.w, h: body.y - c.y };
    let mut lay = Layout { content: c, head, mark, title, addr, status, sub, msg, tabs, range_box, ranges, body, cards: Vec::new(), items: Vec::new(), procs: None, devs: None, parts, total: 0 };
    let area = Rect { x, y: body.y, w, h: body.h };
    if let Some(p) = &m.procs {
        procs(&mut lay, area, p, scroll);
    } else if let Some(d) = &m.devices {
        devices(&mut lay, area, d, scroll);
    } else {
        cards(&mut lay, area, m, scroll);
    }
    lay
}

fn cards(lay: &mut Layout, area: Rect, m: &TabModel, scroll: i32) {
    // Wrap rows whose cards would be narrower than CARD_MIN_W.
    let per = ((area.w + GAP) / (CARD_MIN_W + GAP)).max(1) as usize;
    let mut rows: Vec<(Vec<&Card>, bool)> = Vec::new();
    for r in &m.rows {
        let span: u32 = r.cards.iter().map(|c| c.span).sum();
        if span as usize <= per {
            rows.push((r.cards.iter().collect(), r.grow));
        } else {
            // As even as it goes: 6 cards where 5 fit make two rows of 3.
            let lines = r.cards.len().div_ceil(per);
            for chunk in r.cards.chunks(r.cards.len().div_ceil(lines).max(1)) {
                rows.push((chunk.iter().collect(), r.grow));
            }
        }
    }
    let widths = |cs: &[&Card]| -> Vec<i32> {
        let span: u32 = cs.iter().map(|c| c.span.max(1)).sum();
        let free = area.w - (cs.len() as i32 - 1) * GAP;
        cs.iter().map(|c| free * c.span.max(1) as i32 / span.max(1) as i32).collect()
    };
    let heights: Vec<i32> = rows.iter().map(|(cs, _)| cs.iter().zip(widths(cs)).map(|(c, w)| card_h(c, w)).max().unwrap_or(0)).collect();
    let used: i32 = heights.iter().sum::<i32>() + (rows.len() as i32 - 1).max(0) * GAP;
    let growers = rows.iter().filter(|r| r.1).count() as i32;
    let extra = if growers > 0 { ((area.h - used) / growers).max(0) } else { 0 };
    let mut y = area.y - scroll;
    let mut parts = Vec::new();
    for ((cs, grow), h) in rows.iter().zip(&heights) {
        let h = h + if *grow { extra } else { 0 };
        let mut x = area.x;
        for (c, w) in cs.iter().zip(widths(cs)) {
            let r = Rect { x, y, w, h };
            let mut ps = Vec::new();
            lay.items.push(place_items(r, &c.items, &mut ps));
            parts.extend(ps);
            lay.cards.push(r);
            x += w + GAP;
        }
        y += h + GAP;
    }
    lay.total = y - GAP - (area.y - scroll);
    let body = lay.body;
    lay.parts.extend(parts.into_iter().filter(|(_, r)| inside(body, *r)));
}

/// Column x and width in a table row of width `w` (Name takes the rest).
pub fn columns(x: i32, w: i32) -> [(i32, i32); 7] {
    let fixed = [120, 80, 100, 80, 90, 90];
    let gap = 12;
    let name_w = (w - fixed.iter().sum::<i32>() - 6 * gap).max(80);
    let mut out = [(x, name_w); 7];
    let mut at = x + name_w + gap;
    for (i, f) in fixed.iter().enumerate() {
        out[i + 1] = (at, *f);
        at += f + gap;
    }
    out
}

fn procs(lay: &mut Layout, area: Rect, p: &Procs, scroll: i32) {
    let dw = DETAIL_W.min(area.w / 3);
    let table = Rect { x: area.x, y: area.y, w: area.w - dw - GAP, h: area.h };
    let detail = Rect { x: table.x + table.w + GAP, y: area.y, w: dw, h: area.h };
    let inner = Rect { x: table.x + 12, y: table.y + 16, w: table.w - 24, h: table.h - 32 };
    let labels: Vec<String> = Filter::ALL.iter().zip(p.counts).map(|(f, n)| format!("{} {n}", f.name())).collect();
    let fw: i32 = labels.iter().map(|l| pill_w(l) + 10).sum();
    let search = Rect { x: inner.x + 6, y: inner.y, w: (inner.w - 12 - fw).max(160), h: 44 };
    let mut filters = Vec::new();
    let mut fx = search.x + search.w + 10;
    for (f, l) in Filter::ALL.iter().zip(&labels) {
        let r = Rect { x: fx, y: inner.y + 3, w: pill_w(l), h: TAB_H };
        filters.push((*f, r));
        lay.parts.push((SysPart::Filter(*f), r));
        fx += r.w + 10;
    }
    lay.parts.push((SysPart::Search, search));
    let hy = search.y + search.h + 10;
    let cols = columns(inner.x + 14, inner.w - 28);
    let mut heads = Vec::new();
    for (k, col) in Column::ALL.iter().enumerate() {
        let r = Rect { x: cols[k].0, y: hy, w: cols[k].1, h: HEAD_ROW_H };
        heads.push((*col, r));
        lay.parts.push((SysPart::Sort(*col), r));
    }
    let rows_area = Rect { x: inner.x, y: hy + HEAD_ROW_H + 4, w: inner.w, h: (inner.y + inner.h - hy - HEAD_ROW_H - 4).max(0) };
    let mut y = rows_area.y - scroll;
    let mut lines = Vec::new();
    for (i, s) in p.sections.iter().enumerate() {
        lines.push((LineAt::Head(i), Rect { x: rows_area.x, y, w: rows_area.w, h: GROUP_H }));
        y += GROUP_H;
        for (j, l) in s.lines.iter().enumerate() {
            let r = Rect { x: rows_area.x, y, w: rows_area.w, h: ROW_H };
            lines.push((LineAt::Line(i, j), r));
            if l.pid != 0 && inside(rows_area, r) {
                lay.parts.push((SysPart::Row(l.pid), r));
            }
            y += ROW_H;
        }
        if s.more > 0 {
            let r = Rect { x: rows_area.x, y, w: rows_area.w, h: ROW_H };
            lines.push((LineAt::More(i), r));
            if inside(rows_area, r) {
                lay.parts.push((SysPart::More, r));
            }
            y += ROW_H;
        }
    }
    let total = y + scroll - rows_area.y;
    lines.retain(|(_, r)| r.y + r.h > rows_area.y && r.y < rows_area.y + rows_area.h);
    // The detail card.
    let di = Rect { x: detail.x + PAD, y: detail.y + PAD, w: detail.w - 2 * PAD, h: detail.h - 2 * PAD };
    let d_name = Rect { x: di.x, y: di.y + 26, w: di.w, h: 34 };
    let d_line = Rect { x: di.x, y: d_name.y + 38, w: di.w, h: 36 };
    let mut ry = d_line.y + d_line.h + 14;
    let n = p.detail.as_ref().map_or(0, |d| d.rows.len());
    let d_rows: Vec<Rect> = (0..n)
        .map(|_| {
            let r = Rect { x: di.x, y: ry, w: di.w, h: 22 };
            ry += 22 + ITEM_GAP;
            r
        })
        .collect();
    let prio_row = Rect { x: di.x, y: ry, w: di.w, h: 34 };
    let pw = 78;
    let mut prios = Vec::new();
    for (k, v) in Vorrang::ALL.iter().enumerate() {
        prios.push((*v, Rect { x: di.x + di.w - 3 * pw - 6 + k as i32 * pw, y: ry + 3, w: pw, h: 28 }));
    }
    let d_who = Rect { x: di.x, y: ry + 34 + ITEM_GAP, w: (text_w("Was hat es gelesen und verbunden?", 14.0) + 24).min(di.w), h: 20 };
    let d_note = Rect { x: di.x, y: di.y + di.h - 36, w: di.w, h: 36 };
    let by = d_note.y - 12 - 42;
    let bw = (di.w - 10) / 2;
    let d_buttons = (Rect { x: di.x, y: by, w: bw, h: 42 }, Rect { x: di.x + bw + 10, y: by, w: bw, h: 42 });
    if let Some(d) = &p.detail {
        for (k, (v, r)) in prios.iter().enumerate() {
            if !matches!(d.prio[k], crate::sys_rules::Gate::No(_)) {
                lay.parts.push((SysPart::Prio(*v), *r));
            }
        }
        lay.parts.push((SysPart::WhoDid, d_who));
        if d.show {
            lay.parts.push((SysPart::Show, d_buttons.0));
        }
        if !matches!(d.end, crate::sys_rules::Gate::No(_)) {
            lay.parts.push((SysPart::End, d_buttons.1));
        }
    }
    // The table scrolls inside its card: the body's end is the table's.
    lay.total = total + (lay.body.h - rows_area.h).max(0);
    lay.procs = Some(ProcLay { table, search, filters, heads, lines, rows_area, detail, d_name, d_line, d_rows, d_prio: (prio_row, prios), d_who, d_buttons, d_note, total });
}

fn devices(lay: &mut Layout, area: Rect, d: &Devices, scroll: i32) {
    let lw = DEV_LIST_W.min(area.w / 3);
    let fw = FACTS_W.min(area.w / 3);
    let list = Rect { x: area.x, y: area.y, w: lw, h: area.h };
    let facts = Rect { x: area.x + area.w - fw, y: area.y, w: fw, h: area.h };
    let detail = Rect { x: list.x + lw + GAP, y: area.y, w: (facts.x - GAP - list.x - lw - GAP).max(100), h: area.h };
    let inner = Rect { x: list.x + 12, y: list.y + 14, w: lw - 24, h: area.h - 28 };
    let mut y = inner.y - scroll;
    let mut lines = Vec::new();
    let mut k = 0;
    for (gi, (_, devs)) in d.groups.iter().enumerate() {
        lines.push((None, gi, Rect { x: inner.x, y, w: inner.w, h: GROUP_H }));
        y += GROUP_H;
        for _ in devs {
            let r = Rect { x: inner.x, y, w: inner.w, h: ROW_H };
            lines.push((Some(k), gi, r));
            if inside(inner, r) {
                lay.parts.push((SysPart::Device(k), r));
            }
            k += 1;
            y += ROW_H;
        }
    }
    let pci = d.pci.map(|_| {
        let r = Rect { x: inner.x, y, w: inner.w, h: ROW_H };
        y += ROW_H;
        r
    });
    lay.total = y + scroll - inner.y + (lay.body.h - inner.h).max(0);
    lines.retain(|(_, _, r)| r.y + r.h > inner.y && r.y < inner.y + inner.h);
    let mut ps = Vec::new();
    let d_items = place_items(detail, &d.detail, &mut ps);
    let f_items = place_items(facts, &d.facts, &mut ps);
    let body = lay.body;
    lay.parts.extend(ps.into_iter().filter(|(_, r)| inside(body, *r)));
    lay.devs = Some(DevLay { list, lines, pci, detail, d_items, facts, f_items });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys_model::{CardRow, card};

    fn model() -> TabModel {
        let c = |n: usize| card(1, (0..n).map(|i| Item::Row(format!("r{i}"), "v".into())).collect());
        TabModel { sub: "Läuft seit 3 Min".into(), rows: vec![CardRow { cards: vec![c(2), c(4), c(1), c(1)], grow: false }, CardRow { cards: vec![c(1), c(1)], grow: true }], ..TabModel::default() }
    }

    #[test]
    fn rows_are_as_high_as_their_highest_card_and_the_last_grows() {
        let c = Rect { x: 0, y: 0, w: 1440, h: 900 };
        let l = layout(c, &model(), Tab::Overview, Some("Alles in Ordnung"), 0);
        assert_eq!(l.cards.len(), 6);
        let h0 = 2 * PAD + 4 * 22 + 3 * ITEM_GAP;
        assert!(l.cards[..4].iter().all(|r| r.h == h0), "{:?}", l.cards);
        assert_eq!(l.cards[4].y, l.cards[0].y + h0 + GAP);
        assert_eq!(l.cards[4].y + l.cards[4].h, l.body.y + l.body.h, "the growing row reaches the body's end");
        assert!(l.cards.iter().all(|r| r.x >= c.x && r.x + r.w <= c.x + c.w));
        // Every tab and range pill is a part, side by side, inside the content.
        assert_eq!(l.tabs.len(), 10);
        assert!(l.tabs.windows(2).all(|w| w[0].1.x + w[0].1.w < w[1].1.x));
        assert_eq!(l.ranges.len(), 3);
        assert!(l.part(SysPart::Tab(Tab::History)).is_some() && l.part(SysPart::Range(Range::Day)).is_some());
        let b = l.range_box.unwrap();
        assert!(b.x + b.w <= c.x + c.w - 16);
    }

    #[test]
    fn narrow_windows_wrap_cards_and_scroll() {
        let c = Rect { x: 0, y: 0, w: 700, h: 500 };
        let l = layout(c, &model(), Tab::Cpu, None, 0);
        assert!(l.cards.iter().all(|r| r.w >= CARD_MIN_W), "{:?}", l.cards);
        assert!(l.max_scroll() > 0);
        let s = layout(c, &model(), Tab::Cpu, None, 60);
        assert_eq!(s.cards[0].y, l.cards[0].y - 60);
        assert!(s.parts.iter().all(|(_, r)| r.y >= s.body.y || r.y < s.head.y + s.head.h), "no part of the scrolled body lies under the head");
    }

    #[test]
    fn every_tab_stays_inside_a_narrow_window() {
        for w in [700, 1000, 1440] {
            for tab in [Tab::Overview, Tab::Procs] {
                let c = Rect { x: 20, y: 10, w, h: 600 };
                let l = layout(c, &model(), tab, Some("Alles in Ordnung"), 0);
                assert_eq!(l.tabs.len(), 10);
                for (t, r) in &l.tabs {
                    assert!(r.x >= c.x && r.x + r.w <= c.x + c.w, "{w}: {t:?} at {r:?}");
                    assert_eq!(l.hit((r.x + r.w / 2) as f64, (r.y + r.h / 2) as f64), Some(SysPart::Tab(*t)), "{w}: {t:?} clickable");
                }
                let last = l.tabs.iter().map(|(_, r)| r.y + r.h).max().unwrap();
                assert!(l.body.y > last, "{w}: the body starts under the last tab row");
                if let Some(b) = l.range_box {
                    assert!(b.x >= c.x && b.x + b.w <= c.x + c.w, "{w}: the range box inside");
                    assert!(l.tabs.iter().all(|(_, r)| !(r.x < b.x + b.w && b.x < r.x + r.w && r.y < b.y + b.h && b.y < r.y + r.h)), "{w}: no tab under the range box");
                }
            }
        }
        let wide = layout(Rect { x: 0, y: 0, w: 1440, h: 900 }, &model(), Tab::Overview, None, 0);
        assert!(wide.tabs.iter().all(|(_, r)| r.y == wide.tabs[0].1.y), "one row where they fit");
    }

    #[test]
    fn fill_puts_the_rest_at_the_bottom() {
        let items = vec![Item::Label("a".into()), Item::Fill, Item::Buttons(vec![("Kopieren".into(), SysPart::Copy, true)])];
        let mut ps = Vec::new();
        let r = Rect { x: 0, y: 0, w: 300, h: 400 };
        let it = place_items(r, &items, &mut ps);
        assert_eq!(it[2].y + it[2].h + ITEM_GAP, r.h - PAD);
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].0, SysPart::Copy);
    }

    #[test]
    fn table_columns_fit() {
        let c = columns(100, 900);
        assert_eq!(c[0].0, 100);
        assert_eq!(c[6].0 + c[6].1, 1000);
        assert!(c.windows(2).all(|w| w[0].0 + w[0].1 < w[1].0));
    }
}
