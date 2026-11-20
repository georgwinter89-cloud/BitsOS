//! SYS-A: the program "System"'s charts, drawn into the page's canvas: lines with an optional area under them, bars with a dashed limit line (bars over it in the
//! warning colour), per-thread columns, and the time axis under a chart. Gaps in a series stay gaps.

use crate::canvas::Canvas;
use crate::chrome_draw::Px;
use crate::layout::Rect;
use crate::sys_model::{Chart, Shape, Tone};
use crate::text::{Family, Font, Weight};
use crate::theme::{Rgba, Theme, alpha};

pub fn colour(t: &Theme, k: Tone) -> Rgba {
    match k {
        Tone::Muted => t.muted,
        Tone::Accent => t.accent,
        Tone::Blue => t.kontext[2],
        Tone::Amber => t.kontext[1],
        Tone::Danger => t.danger,
    }
}

fn soft(t: &Theme, k: Tone) -> Rgba {
    if k == Tone::Accent { t.accent_soft } else { alpha(colour(t, k), 0.25) }
}

/// The top of the scale: the fixed one, else a bit above the highest point (1 when everything is 0 or missing).
pub fn top(ch: &Chart) -> f64 {
    if let Some(m) = ch.max.filter(|m| *m > 0.0) {
        return m;
    }
    let peak = ch.series.iter().flat_map(|s| s.0.iter().flatten()).fold(0.0f64, |a, v| a.max(*v));
    let mark = ch.mark.as_ref().map_or(0.0, |m| m.0);
    let p = peak.max(mark);
    if p <= 0.0 { 1.0 } else { p * 1.15 }
}

pub fn draw(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, ch: &Chart) {
    let plot = Rect { h: ch.h, ..r };
    let b = p.r(plot);
    let (x0, y0, w, h) = (b.x as f32, b.y as f32, b.w as f32, b.h as f32);
    let grid = ch.axis.is_some();
    if grid {
        for k in 1..4 {
            let y = y0 + h * k as f32 / 4.0;
            c.line((x0, y), (x0 + w, y), p.line(), t.line);
        }
    }
    let hi = top(ch);
    let y_of = |v: f64| y0 + h - ((v / hi).clamp(0.0, 1.0) as f32) * h;
    if let Some((v, label)) = &ch.mark {
        let y = y_of(*v);
        let dash = p.v(5.0);
        let mut at = x0;
        while at < x0 + w {
            c.line((at, y), ((at + dash).min(x0 + w), y), p.line(), t.muted);
            at += dash * 2.2;
        }
        let f = t.font(Family::Mono, 11.0, Weight::Regular);
        let lw = f.width(label);
        c.text(&f, b.x + b.w - lw, (y as i32) - f.height() - p.v(2.0) as i32, label, t.muted);
    }
    for (pts, tone, area) in &ch.series {
        if pts.is_empty() {
            continue;
        }
        let n = pts.len();
        let step = if n > 1 { w / (n - 1) as f32 } else { w };
        let x_of = |i: usize| x0 + i as f32 * step;
        let col = colour(t, *tone);
        match ch.shape {
            Shape::Bars => {
                let bw = (w / n as f32 * 0.62).max(p.v(1.0));
                let limit = ch.mark.as_ref().map(|m| m.0);
                for (i, v) in pts.iter().enumerate() {
                    let Some(v) = v else { continue };
                    let y = y_of(*v);
                    let x = x0 + (i as f32 + 0.5) * (w / n as f32) - bw / 2.0;
                    let over = limit.is_some_and(|l| *v > l * 1.5);
                    let bh = (y0 + h - y).max(p.v(1.0));
                    c.rrect(crate::canvas::Box2 { x, y: y0 + h - bh, w: bw, h: bh }, (bw / 4.0).min(p.v(2.0)), if over { t.danger } else { col });
                }
            }
            Shape::Line => {
                let width = p.v(if *area { 2.5 } else { 2.0 });
                for i in 1..n {
                    let (Some(a), Some(z)) = (pts[i - 1], pts[i]) else { continue };
                    let (pa, pz) = ((x_of(i - 1), y_of(a)), (x_of(i), y_of(z)));
                    if *area {
                        c.poly(&[pa, pz, (pz.0, y0 + h), (pa.0, y0 + h)], soft(t, *tone));
                    }
                    c.line(pa, pz, width, col);
                }
                // A single known point (the first second): a dot, so the chart is not empty.
                if let Some((i, Some(v))) = pts.iter().enumerate().rev().find(|(_, v)| v.is_some())
                    && (i == 0 || pts[i - 1].is_none())
                {
                    c.disc(x_of(i), y_of(*v), p.v(2.5), col);
                }
            }
        }
    }
    if let Some(labels) = ch.axis {
        let f = t.font(Family::Mono, 11.0, Weight::Regular);
        let band = p.r(Rect { y: r.y + ch.h + 4, h: 16, ..r });
        let n = labels.len();
        for (k, l) in labels.iter().enumerate() {
            let lw = f.width(l);
            let x = match k {
                0 => band.x,
                k if k == n - 1 => band.x + band.w - lw,
                k => band.x + band.w * k as i32 / (n as i32 - 1) - lw / 2,
            };
            c.text(&f, x, band.y + (band.h - f.height()) / 2, l, t.muted);
        }
    }
}

/// The per-thread columns, eight to a row, each with its number under it.
pub fn threads(c: &mut Canvas, p: &Px, t: &Theme, r: Rect, v: &[Option<f64>], label: &Font) {
    let per = 8usize;
    let rows = v.len().div_ceil(per).max(1);
    let cell_h = r.h / rows as i32;
    let gap = 10;
    let cw = (r.w - gap * (per as i32 - 1)) / per as i32;
    for (i, share) in v.iter().enumerate() {
        let (row, col) = (i / per, i % per);
        let x = r.x + col as i32 * (cw + gap);
        let y = r.y + row as i32 * cell_h;
        let track = Rect { x, y, w: cw, h: cell_h - 22 };
        let tb = p.b(track);
        let rad = p.v(6.0);
        c.rrect(tb, rad, t.panel2);
        let Some(share) = share else {
            // Nothing measured in the interval: a dash, not an empty (0 %) column.
            c.text_mid(label, p.r(Rect { y: track.y + track.h / 2 - 9, h: 18, ..track }), "–", t.muted);
            c.text_mid(label, p.r(Rect { x, y: track.y + track.h + 2, w: cw, h: 18 }), &i.to_string(), t.muted);
            continue;
        };
        let fill = ((share / 100.0).clamp(0.0, 1.0) * track.h as f64).round() as i32;
        if fill > 0 {
            let f = Rect { y: track.y + track.h - fill, h: fill, ..track };
            c.rrect(p.b(f), rad.min(p.v(fill as f32 / 2.0)), t.accent);
        }
        c.text_mid(label, p.r(Rect { x, y: track.y + track.h + 2, w: cw, h: 18 }), &i.to_string(), t.muted);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chart(series: Vec<Vec<Option<f64>>>, max: Option<f64>) -> Chart {
        Chart { series: series.into_iter().map(|s| (s, Tone::Accent, true)).collect(), max, shape: Shape::Line, axis: Some(crate::sys_ring::Range::Minute.axis()), h: 100, mark: None }
    }

    #[test]
    fn the_scale_is_fixed_or_follows_the_peak() {
        assert_eq!(top(&chart(vec![vec![Some(5.0)]], Some(100.0))), 100.0);
        assert!((top(&chart(vec![vec![Some(2.0), None, Some(4.0)]], None)) - 4.6).abs() < 1e-9);
        assert_eq!(top(&chart(vec![vec![None, None]], None)), 1.0, "nothing known: no division by zero");
    }

    #[test]
    fn drawing_a_chart_with_gaps_and_bars_stays_in_its_box() {
        let t = Theme::default();
        let r = Rect { x: 0, y: 0, w: 200, h: 120 };
        let p = Px::new(r, 1.0);
        let mut c = Canvas::new(200, 120);
        draw(&mut c, &p, &t, r, &chart(vec![vec![Some(10.0), None, Some(50.0), Some(90.0)]], Some(100.0)));
        let mut bars = chart(vec![vec![Some(16.0), Some(40.0), None]], None);
        bars.shape = Shape::Bars;
        bars.mark = Some((16.7, "16,7 ms".into()));
        draw(&mut c, &p, &t, r, &bars);
        threads(&mut c, &p, &t, r, &[Some(0.0), Some(50.0), None, Some(100.0), Some(120.0)], &t.font(Family::Mono, 11.0, Weight::Regular));
        // An empty chart and a zero-size box draw nothing and do not panic.
        draw(&mut c, &p, &t, Rect { x: 0, y: 0, w: 0, h: 0 }, &chart(Vec::new(), None));
    }
}
