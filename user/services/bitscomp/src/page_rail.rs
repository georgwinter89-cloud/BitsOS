//! WY-SET: the settings' rail left of the page (Einstellungen-Aussehen.dc.html): the name, the search over all
//! keys, the nine tabs with how many rows each shows (while searching: its hits) and, at its foot, how many keys
//! differ from their default with "Zeigen". Layout in logical pixels (pure) and its drawing; the page's head gets
//! the filter "Alle / Nur geänderte" while a list is shown.

use crate::canvas::Canvas;
use crate::chrome_draw::Px;
use crate::layout::Rect;
use crate::page::Part;
use crate::page_draw::FieldLook;
use crate::page_tabs::TABS;
use crate::text::{Family, Weight};
use crate::theme::Theme;

const TAB_H: i32 = 42;
const TAB_GAP: i32 = 4;
const PAD: i32 = 18;
const FOOT_H: i32 = 96;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rail {
    pub rect: Rect,
    pub title: Rect,
    pub search: Rect,
    pub clear: Option<Rect>,
    pub tabs: Vec<Rect>,
    pub foot: Option<Rect>,
    pub show: Option<Rect>,
    /// The head's filter (Alle, Nur geänderte) while a list is shown.
    pub filter: Option<[Rect; 2]>,
    /// The tab shown (None while a list is shown), each tab's row count and its hits while searching.
    pub tab: Option<usize>,
    pub counts: [usize; 9],
    pub hits: Option<[usize; 9]>,
    pub changed: usize,
    pub changed_only: bool,
    /// The keys there are (the field's "Alle N durchsuchen").
    pub total: usize,
    pub query: String,
}

/// The rail's width in content `c`: the mock-up's 300 at its width, less in a narrow tile.
pub fn width(c: Rect) -> i32 {
    (c.w * 22 / 100).clamp(200, 300)
}

/// What the rail shows; `layout` places it.
pub struct Facts {
    pub tab: Option<usize>,
    pub counts: [usize; 9],
    pub hits: Option<[usize; 9]>,
    pub changed: usize,
    pub changed_only: bool,
    pub total: usize,
    pub query: String,
}

/// The rail in content `c`, and the head's filter right of the title in `head` (the page's head) when listing.
pub fn layout(c: Rect, f: Facts, head: Option<Rect>) -> Rail {
    let w = width(c);
    let rect = Rect { x: c.x, y: c.y, w, h: c.h };
    let inner = |y: i32, h: i32| Rect { x: c.x + PAD, y, w: w - 2 * PAD, h };
    let title = inner(c.y + 26, 34);
    let search = inner(c.y + 78, 42);
    let clear = (!f.query.is_empty()).then(|| Rect { x: search.x + search.w - 38, y: search.y + 5, w: 32, h: 32 });
    let top = search.y + search.h + 16;
    let tabs: Vec<Rect> = (0..TABS.len()).map(|i| inner(top + i as i32 * (TAB_H + TAB_GAP), TAB_H)).collect();
    let end = tabs.last().map_or(top, |r| r.y + r.h);
    let fy = c.y + c.h - PAD - FOOT_H;
    let foot = (f.changed > 0 && fy > end + 16).then(|| inner(fy, FOOT_H));
    let show = foot.map(|b| Rect { x: b.x + 14, y: b.y + 60, w: 90, h: 26 });
    let listing = !f.query.trim().is_empty() || f.changed_only;
    let filter = head.filter(|_| listing).map(|h| {
        let (a, b) = (96, 210);
        let x = h.x + h.w - a - 4 - b;
        [Rect { x, y: h.y, w: a, h: h.h }, Rect { x: x + a + 4, y: h.y, w: b, h: h.h }]
    });
    Rail { rect, title, search, clear, tabs, foot, show, filter, tab: f.tab, counts: f.counts, hits: f.hits, changed: f.changed, changed_only: f.changed_only, total: f.total, query: f.query }
}

impl Rail {
    pub fn parts(&self) -> Vec<(Part, Rect)> {
        let mut v = Vec::new();
        if let Some(r) = self.clear {
            v.push((Part::SearchClear, r));
        }
        v.push((Part::Search, self.search));
        v.extend(self.tabs.iter().enumerate().map(|(i, r)| (Part::Tab(i), *r)));
        if let Some(r) = self.show {
            v.push((Part::Changed, r));
        }
        if let Some([a, b]) = self.filter {
            v.push((Part::Filter(false), a));
            v.push((Part::Filter(true), b));
        }
        v
    }
}

/// The rail over the page's image `c` (`p`: the page's pixels), the field's text while Georg types into it.
pub fn draw(c: &mut Canvas, p: &Px, t: &Theme, r: &Rail, hover: Option<Part>, field: Option<&FieldLook>, live: bool) {
    let rr = p.r(r.rect);
    c.fill(rr.x, rr.y, rr.w, rr.h, t.panel2);
    // The logo of the mock-up: a ring and the accent's dot.
    let lb = p.r(Rect { x: r.title.x + 8, y: r.title.y + 2, w: 30, h: 30 });
    let (cx, cy) = (lb.x as f32 + lb.w as f32 / 2.0, lb.y as f32 + lb.h as f32 / 2.0);
    c.ring(cx, cy, p.v(12.0), p.v(2.6), t.text);
    c.disc(cx, cy, p.v(3.6), t.accent);
    let name = t.font(Family::Display, 21.0, Weight::SemiBold);
    c.text_fit(&name, p.r(Rect { x: r.title.x + 50, w: r.title.w - 50, ..r.title }), crate::page_draw::TITLE, t.text);
    // The search field.
    let typing = field.filter(|f| f.part == Part::Search);
    let sb = p.b(r.search);
    let lit = typing.is_some() || !r.query.is_empty() || hover == Some(Part::Search);
    c.rrect(sb, p.v(14.0), t.panel);
    c.rrect_line(sb, p.v(14.0), p.line(), if lit { t.accent } else { t.line_strong });
    let ib = p.r(Rect { x: r.search.x + 14, y: r.search.y + 13, w: 16, h: 16 });
    let (ix, iy) = (ib.x as f32 + p.v(7.0), ib.y as f32 + p.v(7.0));
    c.ring(ix, iy, p.v(5.0), p.v(1.6), t.muted);
    c.line((ix + p.v(4.0), iy + p.v(4.0)), (ix + p.v(8.0), iy + p.v(8.0)), p.v(1.6), t.muted);
    let ui = t.font(Family::Ui, 14.0, Weight::Regular);
    let band = Rect { x: r.search.x + 40, w: r.search.w - 40 - if r.clear.is_some() { 40 } else { 12 }, ..r.search };
    match typing {
        Some(f) => {
            let b = p.r(band);
            c.text_fit(&ui, b, &f.text, t.text);
            let x = b.x + ui.width(&f.before);
            let ch = ui.height();
            c.fill(x, b.y + (b.h - ch) / 2, p.line().round() as i32 + 1, ch, t.accent);
        }
        None if r.query.is_empty() => {
            c.text_fit(&ui, p.r(band), &format!("Alle {} durchsuchen …", r.total), t.muted);
        }
        None => {
            c.text_fit(&ui, p.r(band), &r.query, t.text);
        }
    }
    if let Some(b) = r.clear {
        let q = p.r(b);
        let (x, y, k) = (q.x as f32 + q.w as f32 / 2.0, q.y as f32 + q.h as f32 / 2.0, p.v(5.0));
        let col = if hover == Some(Part::SearchClear) { t.accent } else { t.muted };
        c.line((x - k, y - k), (x + k, y + k), p.v(1.6), col);
        c.line((x - k, y + k), (x + k, y - k), p.v(1.6), col);
    }
    // The tabs.
    let tab_font = t.font(Family::Ui, 15.0, Weight::Medium);
    let mono = t.font(Family::Mono, 11.0, Weight::Medium);
    for (i, b) in r.tabs.iter().enumerate() {
        let on = r.tab == Some(i);
        let bb = p.b(*b);
        if on {
            c.rrect(bb, p.v(14.0), t.accent_soft);
            c.rrect_line(bb, p.v(14.0), p.line(), t.chip);
        } else if hover == Some(Part::Tab(i)) {
            c.rrect(bb, p.v(14.0), t.panel);
        }
        let text = Rect { x: b.x + 14, w: b.w - 14 - 38, ..*b };
        c.text_fit(&tab_font, p.r(text), TABS[i].title, if on { t.accent } else if live { t.text } else { t.muted });
        let num = Rect { x: b.x + b.w - 40, w: 28, ..*b };
        match r.hits {
            Some(h) if h[i] > 0 => {
                let s = h[i].to_string();
                let pw = (mono.width(&s) as f64 / t.scale) as i32 + 16;
                let pb = Rect { x: b.x + b.w - 14 - pw, y: b.y + (b.h - 18) / 2, w: pw, h: 18 };
                c.rrect(p.b(pb), p.v(9.0), t.accent);
                c.text_mid(&mono, p.r(pb), &s, t.on_accent);
            }
            Some(_) => {}
            None => c.text_right(&mono, p.r(num), &r.counts[i].to_string(), t.muted),
        }
    }
    // The foot: how many keys differ from their default.
    if let (Some(b), Some(s)) = (r.foot, r.show) {
        c.rrect(p.b(b), p.v(14.0), t.panel);
        let small = t.font(Family::Ui, 13.0, Weight::Regular);
        let dot = p.r(Rect { x: b.x + 14, y: b.y + 18, w: 8, h: 8 });
        c.disc(dot.x as f32 + dot.w as f32 / 2.0, dot.y as f32 + dot.h as f32 / 2.0, p.v(4.0), t.warm);
        let n = r.changed;
        let line = format!("{n} {}", if n == 1 { "Einstellung weicht" } else { "Einstellungen weichen" });
        c.text_fit(&small, p.r(Rect { x: b.x + 30, y: b.y + 12, w: b.w - 40, h: 20 }), &line, t.muted);
        c.text_fit(&small, p.r(Rect { x: b.x + 30, y: b.y + 32, w: b.w - 40, h: 20 }), "vom Standard ab", t.muted);
        let col = if hover == Some(Part::Changed) || r.changed_only { t.text } else { t.accent };
        c.text_fit(&t.font(Family::Ui, 13.0, Weight::SemiBold), p.r(s), if r.changed_only { "Gezeigt" } else { "Zeigen" }, col);
    }
    // The head's filter.
    if let Some([a, b]) = r.filter {
        let ctl = t.font(Family::Ui, 13.0, Weight::Medium);
        let on = |x: bool| x == r.changed_only;
        for (part, rect, text) in [(Part::Filter(false), a, "Alle".to_string()), (Part::Filter(true), b, format!("Nur geänderte · {}", r.changed))] {
            let ison = on(matches!(part, Part::Filter(true)));
            crate::page_draw::pill(c, p, t, rect, &text, ison, hover == Some(part), live, &ctl);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(query: &str, changed: usize) -> Facts {
        Facts { tab: Some(0), counts: [8; 9], hits: None, changed, changed_only: false, total: 210, query: query.into() }
    }

    #[test]
    fn the_rail_places_its_tabs_and_hides_the_foot_without_room() {
        let c = Rect { x: 100, y: 50, w: 1700, h: 1000 };
        let r = layout(c, facts("", 23), Some(Rect { x: 500, y: 74, w: 1200, h: 30 }));
        assert_eq!(r.rect.w, 300);
        assert_eq!(r.tabs.len(), 9);
        assert!(r.tabs.windows(2).all(|w| w[1].y > w[0].y + w[0].h - 1));
        assert!(r.foot.is_some() && r.clear.is_none() && r.filter.is_none(), "no list: no filter");
        let parts: Vec<Part> = r.parts().iter().map(|(p, _)| *p).collect();
        assert!(parts.contains(&Part::Tab(8)) && parts.contains(&Part::Search) && parts.contains(&Part::Changed));
        let short = layout(Rect { h: 560, ..c }, facts("x", 23), Some(Rect { x: 500, y: 74, w: 1200, h: 40 }));
        assert!(short.foot.is_none() && short.show.is_none(), "a short window: no foot over the tabs");
        assert!(short.clear.is_some() && short.filter.is_some(), "a query: the × and the head's filter");
        assert_eq!(layout(c, facts("", 0), None).foot, None, "nothing changed: no foot");
        assert_eq!(width(Rect { w: 600, ..c }), 200);
    }
}
