//! SYS-A: the task manager (search, filters, the sortable table in its parts, the detail card with Vorrang and the
//! two buttons) and the device list of the tab Geräte, in the look of sys_draw.

use crate::canvas::Canvas;
use crate::chrome_draw::{Px, wrap};
use crate::layout::Rect;
use crate::page_draw::FieldLook;
use crate::sys_chart::colour;
use crate::sys_draw::{Fonts, button, draw_items, pill};
use crate::sys_layout::{DevLay, LineAt, ProcLay, columns};
use crate::sys_model3::{Devices, Procs};
use crate::sys_rules::Gate;
use crate::sys_view::SysPart;
use crate::text::{Family, Weight};
use crate::theme::Theme;

pub const PLACEHOLDER: &str = "Prozess suchen …";

fn search(c: &mut Canvas, p: &Px, t: &Theme, f: &Fonts, r: Rect, query: &str, edit: Option<&FieldLook>, hover: bool) {
    let b = p.b(r);
    c.rrect(b, p.v(14.0), t.panel2);
    c.rrect_line(b, p.v(14.0), p.line(), if edit.is_some() || hover { t.accent } else { t.line_strong });
    let pr = p.r(r);
    let (cx, cy) = (pr.x as f32 + p.v(21.0), pr.y as f32 + pr.h as f32 / 2.0 - p.v(1.5));
    c.ring(cx, cy, p.v(5.0), p.v(1.6), t.muted);
    c.line((cx + p.v(3.6), cy + p.v(3.6)), (cx + p.v(7.5), cy + p.v(7.5)), p.v(1.6), t.muted);
    let band = Rect { x: pr.x + p.v(38.0) as i32, w: pr.w - p.v(50.0) as i32, ..pr };
    let text = edit.map_or(query, |e| e.text.as_str());
    if text.is_empty() && edit.is_none() {
        c.text_fit(&f.body, band, PLACEHOLDER, t.muted);
    } else {
        c.text_fit(&f.body, band, text, t.text);
    }
    if let Some(e) = edit {
        let x = band.x + f.body.width(&e.before);
        let h = f.body.height();
        c.fill(x, band.y + (band.h - h) / 2, p.line().round() as i32 + 1, h, t.accent);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn procs(c: &mut Canvas, p: &Px, t: &Theme, f: &Fonts, l: &ProcLay, m: &Procs, hover: Option<SysPart>, field: Option<&FieldLook>) {
    search(c, p, t, f, l.search, &m.query, field, hover == Some(SysPart::Search));
    for ((flt, r), n) in l.filters.iter().zip(m.counts) {
        pill(c, p, t, *r, &format!("{} {n}", flt.name()), *flt == m.filter, hover == Some(SysPart::Filter(*flt)), &f.ctl);
    }
    for (k, (col, r)) in l.heads.iter().enumerate() {
        let arrow = if *col == m.sort { if m.up { " ▴" } else { " ▾" } } else { "" };
        let text = format!("{}{arrow}", col.head().to_uppercase());
        let colr = if hover == Some(SysPart::Sort(*col)) || *col == m.sort { t.text } else { t.muted };
        if k >= 2 {
            c.text_right(&f.label, p.r(*r), &text, colr);
        } else {
            c.text_fit(&f.label, p.r(*r), &text, colr);
        }
    }
    if !m.loaded {
        c.text_fit(&f.small, p.r(Rect { h: 30, ..l.rows_area }), "Die Prozesse werden gelesen …", t.muted);
    }
    let cols = columns(l.rows_area.x + 14, l.rows_area.w - 28);
    let clip = l.rows_area;
    let bold = t.font(Family::Ui, 14.0, Weight::SemiBold);
    for (at, r) in &l.lines {
        // A line half under the table's top or bottom edge is not drawn (no clipping in the canvas).
        if r.y < clip.y || r.y + r.h > clip.y + clip.h {
            continue;
        }
        match at {
            LineAt::Head(i) => {
                let s = &m.sections[*i];
                c.text_fit(&t.font(Family::Ui, 13.0, Weight::SemiBold), p.r(Rect { x: r.x + 14, y: r.y + 12, w: r.w - 28, h: 20 }), &s.head, t.muted);
            }
            LineAt::Line(i, j) => {
                let ln = &m.sections[*i].lines[*j];
                let b = p.b(*r);
                if ln.selected {
                    c.rrect(b, p.v(14.0), t.accent_soft);
                    c.rrect_line(b, p.v(14.0), p.line(), crate::theme::alpha(t.accent, 0.45));
                } else if hover == Some(SysPart::Row(ln.pid)) {
                    c.rrect(b, p.v(14.0), t.chip);
                }
                let band = |k: usize| p.r(Rect { x: cols[k].0, y: r.y, w: cols[k].1, h: r.h });
                let nb = band(0);
                let w = c.text_fit(if ln.selected { &bold } else { &f.body }, nb, &ln.name, t.text);
                if !ln.note.is_empty() {
                    c.text_fit(&f.small, Rect { x: nb.x + w, w: (nb.w - w).max(0), ..nb }, &format!(" · {}", ln.note), t.muted);
                }
                c.text_fit(&f.body, band(1), &ln.state, colour(t, ln.tone));
                for (k, v) in ln.cells.iter().enumerate() {
                    c.text_right(&f.mono, band(k + 2), v, t.text);
                }
            }
            LineAt::More(i) => {
                let n = m.sections[*i].more;
                let col = if hover == Some(SysPart::More) { t.text } else { t.muted };
                c.text_fit(&f.small, p.r(Rect { x: r.x + 14, w: r.w - 28, ..*r }), &format!("… {n} weitere"), col);
            }
        }
    }
    detail(c, p, t, f, l, m, hover);
}

fn detail(c: &mut Canvas, p: &Px, t: &Theme, f: &Fonts, l: &ProcLay, m: &Procs, hover: Option<SysPart>) {
    let top = Rect { x: l.d_name.x, y: l.detail.y + 24, w: l.d_name.w, h: 16 };
    c.text_fit(&f.label, p.r(top), "AUSGEWÄHLT", t.muted);
    let Some(d) = &m.detail else {
        c.text_fit(&f.body, p.r(l.d_name), "Wähle einen Prozess aus.", t.muted);
        return;
    };
    c.text_fit(&t.font(Family::Display, 26.0, Weight::SemiBold), p.r(l.d_name), &d.name, t.text);
    let lb = p.r(l.d_line);
    for (k, line) in wrap(&f.small, &d.line, lb.w).iter().take(2).enumerate() {
        c.text_fit(&f.small, Rect { y: lb.y + k as i32 * lb.h / 2, h: lb.h / 2, ..lb }, line, t.muted);
    }
    let rule = p.r(Rect { y: l.d_line.y + l.d_line.h + 6, h: 1, ..l.d_line });
    c.fill(rule.x, rule.y, rule.w, p.line().round() as i32, t.line);
    for ((k, v), r) in d.rows.iter().zip(&l.d_rows) {
        let b = p.r(*r);
        let vw = f.body.width(v).min(b.w * 3 / 5);
        c.text_right(&f.body, Rect { x: b.x + b.w - vw, w: vw, ..b }, v, t.text);
        c.text_fit(&f.small, Rect { w: b.w - vw - 8, ..b }, k, t.muted);
    }
    let (row, prios) = &l.d_prio;
    c.text_fit(&f.small, p.r(Rect { w: row.w / 3, ..*row }), "Vorrang", t.muted);
    if let (Some(first), Some(last)) = (prios.first(), prios.last()) {
        let bx = Rect { x: first.1.x - 3, y: row.y, w: last.1.x + last.1.w - first.1.x + 6, h: row.h };
        let bb = p.b(bx);
        c.rrect(bb, bb.h / 2.0, t.panel2);
    }
    for (k, (v, r)) in prios.iter().enumerate() {
        let free = !matches!(d.prio[k], Gate::No(_));
        let on = *v == d.vorrang;
        if free {
            pill(c, p, t, *r, v.name(), on, hover == Some(SysPart::Prio(*v)), &f.small);
        } else {
            c.text_mid(&f.small, p.r(*r), v.name(), if on { t.text } else { t.line_strong });
        }
    }
    let who = hover == Some(SysPart::WhoDid);
    c.text_fit(&f.ctl, p.r(l.d_who), "Was hat es gelesen und verbunden? →", if who { t.text } else { t.accent });
    let (show, end) = l.d_buttons;
    button(c, p, t, show, "Fenster zeigen", false, d.show, hover == Some(SysPart::Show), &f.ctl);
    button(c, p, t, end, "Beenden", true, !matches!(d.end, Gate::No(_)), hover == Some(SysPart::End), &f.ctl);
    let nb = p.r(l.d_note);
    for (k, line) in wrap(&f.small, &d.note, nb.w).iter().take(2).enumerate() {
        c.text_fit(&f.small, Rect { y: nb.y + k as i32 * nb.h / 2, h: nb.h / 2, ..nb }, line, t.muted);
    }
}

pub fn devices(c: &mut Canvas, p: &Px, t: &Theme, f: &Fonts, l: &DevLay, m: &Devices, hover: Option<SysPart>) {
    let clip = Rect { y: l.list.y + 10, h: l.list.h - 20, ..l.list };
    let mut names = Vec::new();
    for (_, devs) in &m.groups {
        names.extend(devs.iter());
    }
    for (dev, gi, r) in &l.lines {
        if r.y < clip.y || r.y + r.h > clip.y + clip.h {
            continue;
        }
        match dev {
            None => {
                let head = m.groups.get(*gi).map_or("", |g| g.0.as_str());
                c.text_fit(&t.font(Family::Ui, 13.0, Weight::SemiBold), p.r(Rect { x: r.x + 14, y: r.y + 12, w: r.w - 28, h: 20 }), head, t.muted);
            }
            Some(k) => {
                let Some((name, state, tone)) = names.get(*k) else { continue };
                let b = p.b(*r);
                if *k == m.selected {
                    c.rrect(b, p.v(14.0), t.accent_soft);
                    c.rrect_line(b, p.v(14.0), p.line(), crate::theme::alpha(t.accent, 0.45));
                } else if hover == Some(SysPart::Device(*k)) {
                    c.rrect(b, p.v(14.0), t.chip);
                }
                let band = p.r(Rect { x: r.x + 14, w: r.w - 28, ..*r });
                let sw = f.small.width(state).min(band.w / 2);
                c.text_right(&f.small, Rect { x: band.x + band.w - sw, w: sw, ..band }, state, colour(t, *tone));
                c.text_fit(&f.body, Rect { w: band.w - sw - 10, ..band }, name, t.text);
            }
        }
    }
    if let (Some(r), Some(n)) = (l.pci, m.pci)
        && r.y + r.h <= clip.y + clip.h
    {
        c.text_fit(&f.small, p.r(Rect { x: r.x + 14, w: r.w - 28, ..r }), &format!("{n} Geräte am PCI-Bus"), t.muted);
    }
    draw_items(c, p, t, f, &m.detail, &l.d_items, hover, l.detail);
    draw_items(c, p, t, f, &m.facts, &l.f_items, hover, l.facts);
}
