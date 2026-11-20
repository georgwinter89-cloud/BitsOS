//! REWIND stutter (Georg 13.11.2026): the view's picture is drawn once when it opens and then only where it changed.
//! Drawing all of it at 2560x1440 took about 30 ms on the main loop for each hover or choice (the pointer stuttered
//! there and nowhere else), and each time it made a new 15 MB picture, a copy of it and a new texture: allocations
//! the kernel met with huge-page faults, compaction and TLB shootdowns. Now one buffer stays while the view is
//! open; a change compares the old look with the new one part by part, draws each changed rectangle on its own in
//! bands of at most `BAND` rows (small allocations), copies it into the buffer and names it as the damage, so the
//! renderer uploads only that.

use std::time::Instant;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::utils::{Buffer, Rectangle, Transform};

use super::{Look, draw, look};
use crate::chrome_draw::Px;
use crate::layout::Rect;
use crate::rewind::Hot;
use crate::state::App;
use crate::theme::Theme;

/// The tallest band drawn at once (logical px).
const BAND: i32 = 160;
/// What a part's rectangle grows by for what is drawn around it (antialiasing, a focus ring, a glyph's overhang).
const PAD: i32 = 8;

fn union(a: Rect, b: Rect) -> Rect {
    let (x, y) = (a.x.min(b.x), a.y.min(b.y));
    Rect { x, y, w: (a.x + a.w).max(b.x + b.w) - x, h: (a.y + a.h).max(b.y + b.h) - y }
}

fn clip(r: Rect, to: Rect) -> Option<Rect> {
    let (x, y) = (r.x.max(to.x), r.y.max(to.y));
    let (x1, y1) = ((r.x + r.w).min(to.x + to.w), (r.y + r.h).min(to.y + to.h));
    (x1 > x && y1 > y).then_some(Rect { x, y, w: x1 - x, h: y1 - y })
}

/// Where a hovered part lies (rows and chips carry their hover in their own looks).
fn hot_rect(l: &Look, h: Hot) -> Option<Rect> {
    let (g, p) = (&l.g, &l.pane);
    Some(match h {
        Hot::Back => g.back,
        Hot::Now => g.now,
        Hot::Older => g.older,
        Hot::Newer => g.newer,
        Hot::Way(t) => g.ways[crate::rewind::Tab::WAYS.iter().position(|w| *w == t)?],
        Hot::Up => p.up,
        Hot::Do | Hot::Restart => p.act,
        Hot::Undo => p.act2,
        Hot::DialogCancel | Hot::DialogOk => {
            let (_, cancel, ok) = crate::page_confirm::boxes(g.full, l.dialog.as_ref()?.lines.len());
            if h == Hot::DialogOk { ok } else { cancel }
        }
        Hot::Chip(_) | Hot::Row(_) => return None,
    })
}

/// The rectangles (logical) where `new` differs from `old`; None: all of it.
pub(super) fn changed(old: &Look, new: &Look) -> Option<Vec<Rect>> {
    let dialog = |l: &Look| l.dialog.clone().map(|mut d| {
        d.on_ok = false;
        d
    });
    let (op, np) = (&old.pane, &new.pane);
    if old.g != new.g || old.theme != new.theme || old.tab != new.tab || old.avail != new.avail || dialog(old) != dialog(new) || (op.up, op.head, &op.rows) != (np.up, np.head, &np.rows) {
        return None;
    }
    let g = &new.g;
    let mut v = Vec::new();
    let mut add = |yes: bool, r: Rect| {
        if yes && r.w > 0 && r.h > 0 {
            v.push(r);
        }
    };
    add(old.date != new.date, g.date);
    add((&old.time, &old.ago, &old.kind) != (&new.time, &new.ago, &new.kind), union(g.label, union(g.time, union(g.ago, g.chip))));
    if old.chips.len() != new.chips.len() {
        add(true, g.lower);
    } else {
        for (a, b) in old.chips.iter().zip(&new.chips) {
            add(a != b, union(a.rect, b.rect));
        }
    }
    if old.rows.len() != new.rows.len() || old.lines != new.lines {
        add(true, g.pane);
    } else {
        for (a, b) in old.rows.iter().zip(&new.rows) {
            add(a != b, union(a.rect, b.rect));
        }
    }
    add((&old.head, old.up) != (&new.head, new.up), union(np.up, np.head));
    if (&old.says, &old.act) != (&new.says, &new.act) {
        for r in [op.says, op.act, op.act2, np.says, np.act, np.act2] {
            add(true, r);
        }
    }
    add(old.foot != new.foot, g.foot);
    if old.hover != new.hover {
        for h in [old.hover, new.hover].into_iter().flatten() {
            if let Some(r) = hot_rect(old, h).into_iter().chain(hot_rect(new, h)).reduce(union) {
                add(true, r);
            }
        }
    }
    if old.dialog.as_ref().map(|d| d.on_ok) != new.dialog.as_ref().map(|d| d.on_ok) {
        for h in [Hot::DialogCancel, Hot::DialogOk] {
            if let Some(r) = hot_rect(new, h) {
                add(true, r);
            }
        }
    }
    let grown: Vec<Rect> = v.into_iter().filter_map(|r| clip(Rect { x: r.x - PAD, y: r.y - PAD, w: r.w + 2 * PAD, h: r.h + 2 * PAD }, g.full)).collect();
    // Most of the view changed: once, all of it.
    let area: i64 = grown.iter().map(|r| r.w as i64 * r.h as i64).sum();
    (area * 5 < g.full.w as i64 * g.full.h as i64 * 3).then_some(grown)
}

/// Draws `parts` of look `l` into `buf` (`size`: its pixels) band by band; returns the pixels drawn.
fn into(buf: &mut MemoryRenderBuffer, size: (i32, i32), l: &Look, t: &Theme, parts: &[Rect]) -> u64 {
    let full = Px::new(l.g.full, t.scale);
    let mut drawn = 0;
    let mut ctx = buf.render();
    for part in parts {
        let mut y = part.y;
        while y < part.y + part.h {
            let band = Rect { y, h: BAND.min(part.y + part.h - y), ..*part };
            y += band.h;
            let c = draw(l, t, band);
            let o = full.r(band);
            let bytes = c.bytes();
            let _ = ctx.draw(|mem: &mut [u8]| -> Result<Vec<Rectangle<i32, Buffer>>, ()> {
                let (x0, y0) = (o.x.max(0), o.y.max(0));
                let (x1, y1) = ((o.x + c.w).min(size.0), (o.y + c.h).min(size.1));
                if x1 <= x0 || y1 <= y0 {
                    return Ok(Vec::new());
                }
                let n = (x1 - x0) as usize * 4;
                for row in y0..y1 {
                    let src = (((row - o.y) * c.w + (x0 - o.x)) * 4) as usize;
                    let dst = ((row * size.0 + x0) * 4) as usize;
                    mem[dst..dst + n].copy_from_slice(&bytes[src..src + n]);
                }
                drawn += (n / 4) as u64 * (y1 - y0) as u64;
                Ok(vec![Rectangle::new((x0, y0).into(), (x1 - x0, y1 - y0).into())])
            });
        }
    }
    drawn
}

/// Brings the view's picture up to date (nothing to do when the look is the same); false while the view is closed.
pub fn paint(app: &mut App) -> bool {
    if !app.rewind.open {
        app.rewind.images = Default::default();
        return false;
    }
    let t = app.theme.clone();
    let l = look(app);
    let size = Px::new(l.g.full, t.scale).size();
    let im = &mut app.rewind.images;
    let parts = match &im.view {
        Some((o, _, s)) if *s == size && *o == l => return true,
        Some((o, _, s)) if *s == size => changed(o, &l).unwrap_or_else(|| vec![l.g.full]),
        _ => {
            im.view = Some((l.clone(), MemoryRenderBuffer::new(Fourcc::Argb8888, size, 1, Transform::Normal, None), size));
            vec![l.g.full]
        }
    };
    let start = Instant::now();
    let Some((old, buf, _)) = im.view.as_mut() else { return true };
    let px = into(buf, size, &l, &t, &parts);
    *old = l;
    let ms = start.elapsed().as_secs_f64() * 1e3;
    im.draws += 1;
    im.draw_max_ms = im.draw_max_ms.max(ms);
    im.draw_px += px;
    true
}

/// Tests: the picture's bytes as they are now.
#[cfg(test)]
pub fn bytes(app: &App) -> Option<Vec<u8>> {
    let (_, buf, _) = app.rewind.images.view.as_ref()?;
    let mut buf = buf.clone();
    let mut out = Vec::new();
    let _ = buf.render().draw(|mem: &mut [u8]| -> Result<Vec<Rectangle<i32, Buffer>>, ()> {
        out = mem.to_vec();
        Ok(Vec::new())
    });
    Some(out)
}

/// Tests: the whole view drawn at once, as it was before this file (what every redraw must match).
#[cfg(test)]
pub fn whole(app: &App) -> Vec<u8> {
    let l = look(app);
    draw(&l, &app.theme, l.g.full).bytes()
}
