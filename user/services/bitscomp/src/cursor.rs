//! The pointer bitscomp draws itself where no host cursor exists (fbdev on V2) and the hardware cursor cannot
//! show it (WY-11 `hwcursor`: QEMU, Linux, a refused shape): a client's cursor surface at its hotspot, else the
//! named shape from the Xcursor theme (`cursor_theme`; the built-in arrow when the theme lacks it). Drawn as the
//! topmost element, so the damage tracker repaints only the old and new spots on a move.

use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement};
use smithay::backend::renderer::element::surface::render_elements_from_surface_tree;
use smithay::backend::renderer::pixman::PixmanRenderer;
use smithay::backend::allocator::Fourcc;
use smithay::input::pointer::CursorImageSurfaceData;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::compositor::with_states;

use crate::render::CompElement;
use crate::state::App;

/// B black outline, W white body, space transparent; the hot spot is the top left pixel.
const ARROW: [&str; 19] = [
    "B", "BB", "BWB", "BWWB", "BWWWB", "BWWWWB", "BWWWWWB", "BWWWWWWB", "BWWWWWWWB", "BWWWWWWWWB", "BWWWWWWWWWB",
    "BWWWWWWBBBBB", "BWWWBWWB", "BWWBBWWB", "BWB BWWB", "BB   BWWB", "B     BWWB", "      BWWB", "       BB",
];

/// The smallest size >= `n` that is a whole number of logical points at scale `s` (the element is then shown
/// 1:1, without pixman's filter blurring the outline).
pub fn padded(n: i32, s: f64) -> i32 {
    (n..n + 16).find(|&m| ((m as f64 / s) - (m as f64 / s).round()).abs() < 1e-6).unwrap_or(n)
}

/// The arrow at `k` times its size in a transparent `w` x `h` buffer, as ARGB pixels (w, h, bytes).
pub fn arrow(k: i32, w: i32, h: i32) -> (i32, i32, Vec<u8>) {
    let (w, h) = (w.max(12 * k), h.max(ARROW.len() as i32 * k));
    let mut px = vec![0u32; (w * h) as usize];
    for (y, row) in ARROW.iter().enumerate() {
        for (x, c) in row.bytes().enumerate() {
            let p = match c {
                b'B' => 0xff00_0000,
                b'W' => 0xffff_ffff,
                _ => continue,
            };
            for dy in 0..k {
                for dx in 0..k {
                    px[((y as i32 * k + dy) * w + x as i32 * k + dx) as usize] = p;
                }
            }
        }
    }
    (w, h, px.iter().flat_map(|p| p.to_le_bytes()).collect())
}

/// The pointer's elements (topmost first), empty when it is hidden or the host draws it; WY-7's "starting" mark
/// beside it while a started program has no window yet (shown also when a client hid the pointer, and in software
/// beside a hardware cursor).
pub fn elements(app: &mut App, pixman: &mut PixmanRenderer) -> Vec<CompElement> {
    if !app.draw_pointer {
        return Vec::new();
    }
    let mut out = if app.cursor.on_hw { Vec::new() } else { pointer(app, pixman) };
    if app.launches.busy() {
        out.extend(starting(app, pixman));
    }
    out
}

fn starting(app: &mut App, pixman: &mut PixmanRenderer) -> Option<CompElement> {
    let s = app.theme.scale;
    let d = padded((16.0 * s).round() as i32, s);
    if app.mark_img.as_ref().is_none_or(|(dd, _)| *dd != d) {
        let bytes = crate::launch::mark(d);
        app.mark_img = Some((d, MemoryRenderBuffer::from_slice(&bytes, Fourcc::Argb8888, (d, d), 1, Transform::Normal, None)));
    }
    let (_, buf) = app.mark_img.as_ref()?;
    let src: Rectangle<f64, Logical> = Rectangle::from_size(Size::from((d as f64, d as f64)));
    let size: Size<i32, Logical> = ((d as f64 / s).round() as i32, (d as f64 / s).round() as i32).into();
    // Below right of the hot spot, clear of the arrow's tail: at least the built-in arrow's (14,16), further out
    // for a bigger theme image (its extent right of and below the hotspot, logical).
    let (ox, oy) = app.cursor.img.as_ref().map_or((0.0, 0.0), |i| {
        (i.w.saturating_sub(i.hot.0) as f64 * 0.55 / s, i.h.saturating_sub(i.hot.1) as f64 * 0.75 / s)
    });
    let at = app.pointer + Point::from((ox.max(14.0), oy.max(16.0)));
    let at = at.to_physical(s);
    MemoryRenderBufferRenderElement::from_buffer(pixman, Point::from((at.x.round(), at.y.round())), buf, None, Some(src), Some(size), Kind::Cursor)
        .ok()
        .map(CompElement::from)
}

fn pointer(app: &mut App, pixman: &mut PixmanRenderer) -> Vec<CompElement> {
    if app.cursor_hidden && app.cursor_deco.is_none() {
        return Vec::new();
    }
    let s = app.theme.scale;
    let at = app.pointer.to_physical(s);
    if app.cursor_deco.is_none()
        && let Some(surface) = app.cursor_surface.clone()
        && surface.is_alive()
    {
        let hot = with_states(&surface, |st| {
            st.data_map.get::<CursorImageSurfaceData>().map(|d| d.lock().map(|a| a.hotspot).unwrap_or_default()).unwrap_or_default()
        });
        let loc: Point<i32, smithay::utils::Physical> = (at - hot.to_f64().to_physical(s)).to_i32_round();
        return render_elements_from_surface_tree(pixman, &surface, loc, s, 1.0, Kind::Cursor);
    }
    // WY-11: the named shape's theme image (`hwcursor::cursor_sync` keeps it current), 1:1 in output pixels.
    let Some(img) = app.cursor.img.clone() else { return Vec::new() };
    let bits = s.to_bits();
    if app.pointer_img.as_ref().is_none_or(|(i, b, _, _)| !std::rc::Rc::ptr_eq(i, &img) || *b != bits) {
        let (w, h) = (padded(img.w as i32, s), padded(img.h as i32, s));
        let mut px = vec![0u32; (w * h) as usize];
        for y in 0..img.h as usize {
            let row = &img.px[y * img.w as usize..][..img.w as usize];
            px[y * w as usize..][..img.w as usize].copy_from_slice(row);
        }
        let bytes: Vec<u8> = px.iter().flat_map(|p| p.to_le_bytes()).collect();
        let buf = MemoryRenderBuffer::from_slice(&bytes, Fourcc::Argb8888, (w, h), 1, Transform::Normal, None);
        app.pointer_img = Some((img.clone(), bits, (w, h), buf));
    }
    let Some((_, _, (w, h), buf)) = app.pointer_img.as_ref() else { return Vec::new() };
    let (w, h) = (*w, *h);
    let src: Rectangle<f64, Logical> = Rectangle::from_size(Size::from((w as f64, h as f64)));
    // Shown at one buffer pixel per output pixel whatever the scale.
    let size: Size<i32, Logical> = ((w as f64 / s).round() as i32, (h as f64 / s).round() as i32).into();
    let loc = Point::from((at.x.round() - img.hot.0 as f64, at.y.round() - img.hot.1 as f64));
    MemoryRenderBufferRenderElement::from_buffer(pixman, loc, buf, None, Some(src), Some(size), Kind::Cursor)
        .ok()
        .map(CompElement::from)
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn arrow_shape() {
        assert_eq!((super::padded(12, 1.25), super::padded(19, 1.25), super::padded(19, 1.0)), (15, 20, 19));
        let (w, h, b) = super::arrow(2, 0, 0);
        assert_eq!((w, h, b.len()), (24, 38, 24 * 38 * 4));
        // Hot spot pixel black and opaque, a body pixel white, the right top corner transparent.
        let px = |x: i32, y: i32| u32::from_le_bytes(b[((y * w + x) * 4) as usize..][..4].try_into().unwrap());
        assert_eq!(px(0, 0), 0xff00_0000);
        assert_eq!(px(3, 8), 0xffff_ffff);
        assert_eq!(px(23, 0), 0);
    }
}
