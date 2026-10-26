//! bitscomp's own drawing around windows (WY-5 look): the header image of `header`, the side and bottom borders,
//! the rounded bottom corners of a tile (images that round off the client's corners), the focused tile's glow
//! lines (theme.effects cheap: two lines at the accent's glow alpha, no blur), and the overlays on top of
//! everything (circle menu, drop preview). Geometry comes from `zones`, colours from `theme`.

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::solid::{SolidColorBuffer, SolidColorRenderElement};
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::{Logical, Physical, Point, Rectangle, Scale, Size};

use crate::canvas::{Box2, Canvas};
use crate::grab::Grab;
use crate::header::{self, GLOW, px_rect};
use crate::layout::{Place, Placement, Rect};
use crate::render::CompElement;
use crate::state::App;
use crate::theme::{Rgba, Theme};
use crate::zones::{self, Drop, Part};
use crate::{menu, tabs};

fn px_loc(x: i32, y: i32, s: f64) -> Point<f64, Physical> {
    Point::from((crate::header::px(x, s) as f64, crate::header::px(y, s) as f64))
}

/// An image element drawn at output resolution: `px` buffer pixels shown over `logical` points.
pub(crate) fn image(pixman: &mut PixmanRenderer, buf: &MemoryRenderBuffer, r: Rect, px: (i32, i32), s: f64) -> Option<CompElement> {
    image_a(pixman, buf, r, px, s, 1.0)
}

/// As `image`, faded by `alpha` (start animation, menus unfolding).
pub(crate) fn image_a(
    pixman: &mut PixmanRenderer,
    buf: &MemoryRenderBuffer,
    r: Rect,
    px: (i32, i32),
    s: f64,
    alpha: f32,
) -> Option<CompElement> {
    let src: Rectangle<f64, Logical> = Rectangle::from_size(Size::from((px.0 as f64, px.1 as f64)));
    let size: Size<i32, Logical> = (r.w, r.h).into();
    let a = (alpha < 1.0).then_some(alpha.max(0.0));
    MemoryRenderBufferRenderElement::from_buffer(pixman, px_loc(r.x, r.y, s), buf, a, Some(src), Some(size), Kind::Unspecified)
        .ok()
        .map(CompElement::from)
}

/// Solid pieces of a frame: (piece number, rect, colour). Tiles leave the bottom corners to the corner images.
fn pieces(t: &Theme, f: Rect, focused: bool, round: bool) -> Vec<(u8, Rect, Rgba)> {
    let edge = if focused { t.accent } else { t.line };
    let r = if round { t.radius.max(1) } else { 0 };
    let top = f.y + t.header_height;
    let side_h = (f.h - t.header_height - r.max(1)).max(1);
    let mut v = vec![
        (1, Rect { x: f.x, y: top, w: 1, h: side_h }, edge),
        (2, Rect { x: f.x + f.w - 1, y: top, w: 1, h: side_h }, edge),
        (3, Rect { x: f.x + r, y: f.y + f.h - 1, w: (f.w - 2 * r).max(1), h: 1 }, edge),
    ];
    if focused {
        // The inner line on top of a two pixel wide outer one, so rounding at 125 % leaves no gap between them.
        for (i, (d, wd, c)) in [(1, 1, t.glow), (2, 2, t.glow_soft)].into_iter().enumerate() {
            let n = 4 + 3 * i as u8;
            v.push((n, Rect { x: f.x - d, y: top, w: wd, h: side_h }, c));
            v.push((n + 1, Rect { x: f.x + f.w, y: top, w: wd, h: side_h }, c));
            v.push((n + 2, Rect { x: f.x + r, y: f.y + f.h, w: (f.w - 2 * r).max(1), h: wd }, c));
        }
    }
    v
}

/// A bottom corner of a tile (`right` or left): the gap colour outside the rounded edge, the border, the glow.
fn corner(t: &Theme, f: Rect, right: bool, focused: bool) -> (Rect, Canvas) {
    let s = t.scale;
    let r = t.radius.max(1);
    let area = Rect { x: if right { f.x + f.w - r } else { f.x - GLOW }, y: f.y + f.h - r, w: r + GLOW, h: r + GLOW };
    let o = px_rect(area, s);
    let p = px_rect(f, s);
    let fb = Box2 { x: (p.x - o.x) as f32, y: (p.y - o.y) as f32, w: p.w as f32, h: p.h as f32 };
    let rad = (t.radius as f64 * s) as f32;
    let mut c = Canvas::new(o.w, o.h);
    c.rrect_outside(fb, rad, t.bg);
    if focused {
        c.glow(fb, rad, if t.effects_gpu { 6 } else { 2 }, t.glow);
    }
    c.rrect_line(fb, rad, (s as f32).max(1.0), if focused { t.accent } else { t.line });
    (area, c)
}

#[derive(Clone, Debug, PartialEq)]
pub struct CornerKey {
    pub frame: Rect,
    pub focused: bool,
    pub theme: u64,
}

/// A window's frame elements, topmost first: the header image, the corners, the borders.
pub fn frame(app: &mut App, pixman: &mut PixmanRenderer, p: &Placement<Window>) -> Vec<CompElement> {
    let Some(id) = p.win.toplevel().map(|t| t.wl_surface().id()) else { return Vec::new() };
    let t = app.theme.clone();
    let s = t.scale;
    let (_, shown) = zones::pages(p.place);
    let hover = app.hover.as_ref().filter(|(w, _)| *w == p.win).map(|(_, part)| *part);
    let tiled = matches!(p.place, Place::Tiled { .. });
    let tile_no = match p.place {
        Place::Tiled { tile, .. } => Some(tile),
        Place::Floating { .. } => None,
    };
    let address = (!t.layout.address_top).then(|| {
        let edit = app.desk.edit.as_ref().filter(|e| e.tile.is_some() && e.tile == tile_no);
        let text = match edit {
            Some(e) => e.text.clone(),
            None => app.address_of(&p.win),
        };
        (text, edit.map(|e| e.before().to_string()))
    });
    let look = header::Look {
        frame: p.frame,
        focused: p.focused,
        floating: !tiled,
        maximised: match p.place {
            Place::Floating { index } => app.ws.floating.get(index).is_some_and(|f| f.restore.is_some()),
            Place::Tiled { .. } => false,
        },
        labels: header::labels(app, &p.win, p.place),
        shown,
        hover,
        address,
        hover_address: hover == Some(Part::Address),
        theme: app.theme_gen,
    };
    let entry = app.deco_img.entry(id.clone());
    let entry = entry.or_insert_with(|| (look.clone(), header::draw(&look, &t).into_buffer()));
    if entry.0 != look {
        *entry = (look.clone(), header::draw(&look, &t).into_buffer());
    }
    let ir = header::image_rect(p.frame, &t);
    let hr = px_rect(ir, s);
    let mut out: Vec<CompElement> = image(pixman, &entry.1, ir, (hr.w, hr.h), s).into_iter().collect();
    if tiled {
        let key = CornerKey { frame: p.frame, focused: p.focused, theme: app.theme_gen };
        for right in [false, true] {
            let e = app.corner_img.entry((id.clone(), right)).or_insert_with(|| {
                let (r, c) = corner(&t, p.frame, right, p.focused);
                (key.clone(), r, c.into_buffer())
            });
            if e.0 != key {
                let (r, c) = corner(&t, p.frame, right, p.focused);
                *e = (key.clone(), r, c.into_buffer());
            }
            let pr = px_rect(e.1, s);
            out.extend(image(pixman, &e.2, e.1, (pr.w, pr.h), s));
        }
    }
    for (part, rect, color) in pieces(&t, p.frame, p.focused, tiled) {
        let buf = app.deco.entry((id.clone(), part)).or_insert_with(|| SolidColorBuffer::new((1, 1), color));
        buf.update((rect.w, rect.h), color);
        let loc: Point<i32, Physical> = px_loc(rect.x, rect.y, s).to_i32_round();
        out.push(SolidColorRenderElement::from_buffer(buf, loc, Scale::from(s), 1.0, Kind::Unspecified).into());
    }
    out
}

/// The circle menu and the drop preview, topmost first.
pub fn overlays(app: &mut App, pixman: &mut PixmanRenderer) -> Vec<CompElement> {
    let t = app.theme.clone();
    let s = t.scale;
    let mut out = Vec::new();
    if let Some(m) = &app.menu {
        let key = (m.items, m.hover, app.theme_gen);
        if app.menu_img.as_ref().is_none_or(|(k, _)| *k != key) {
            app.menu_img = Some((key, crate::menu_draw::menu(&m.items, m.hover, &t).into_buffer()));
        }
        let (ex, ey) = menu::extent(&t);
        let r = Rect { x: m.centre.0 as i32 - ex, y: m.centre.1 as i32 - ey, w: 2 * ex, h: 2 * ey };
        let px = ((r.w as f64 * s).round() as i32, (r.h as f64 * s).round() as i32);
        let a = app.anim.menu_alpha(crate::pacer::now(), &t);
        out.extend(image_a(pixman, &app.menu_img.as_ref().unwrap().1, r, px, s, a));
    }
    if let Some(Grab::MoveTile { drop, .. }) = &app.grab {
        let frames = app.ws.tile_frames(app.area(), t.gap);
        let preview = match *drop {
            Drop::Reorder { tile, slot, .. } => frames.get(tile).and_then(|&f| {
                let (n, shown) = (app.ws.tiles[tile].stack.len(), app.ws.tiles[tile].top);
                tabs::slot_bar(&tabs::strip(f, n, shown, &t), slot, &t)
            }),
            d => zones::drop_preview(&frames, d, app.area(), t.gap),
        };
        if let Some(r) = preview {
            let color = if matches!(drop, Drop::Reorder { .. }) { t.accent } else { t.glow };
            app.preview_buf.update((r.w, r.h), color);
            let loc: Point<i32, Physical> = px_loc(r.x, r.y, s).to_i32_round();
            let e = SolidColorRenderElement::from_buffer(&app.preview_buf, loc, Scale::from(s), 1.0, Kind::Unspecified);
            out.push(e.into());
        }
    }
    out
}
