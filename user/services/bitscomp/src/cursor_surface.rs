//! WY-11: a client's cursor surface (wl_pointer.set_cursor) as one hardware cursor image. Only the plain case goes
//! to the hardware: one wl_shm buffer in Argb/Xrgb8888, no buffer transform, no viewport, no subsurfaces, and a
//! result that fits the engine (HW_MAX) once scaled to the output. Anything else is drawn in software, where
//! smithay's renderer honours all of that, so the pointer looks the same either way. The size is checked before
//! a single pixel is copied: a huge buffer costs nothing (WY-11 fixer).

use std::rc::Rc;

use smithay::backend::renderer::utils::{SurfaceView, with_renderer_surface_state};
use smithay::input::pointer::CursorImageSurfaceData;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Size, Transform};
use smithay::wayland::compositor::{get_children, with_states};
use smithay::wayland::shm::with_buffer_contents;

use crate::cursor_theme::{CursorImage, scale_nearest};
use crate::hwcursor::{HW_MAX, Want};

/// The surface's hotspot (surface-local, logical), as the client last set it.
pub fn hotspot(surface: &WlSurface) -> (i32, i32) {
    with_states(surface, |st| {
        st.data_map
            .get::<CursorImageSurfaceData>()
            .map(|d| d.lock().map(|a| (a.hotspot.x, a.hotspot.y)).unwrap_or_default())
            .unwrap_or_default()
    })
}

/// Shown 1:1 from its buffer: no transform, and a viewport (if any) that neither crops nor scales.
pub fn plain(transform: Transform, view: Option<SurfaceView>, buffer: Option<Size<i32, Logical>>) -> bool {
    let Some(b) = buffer else { return false };
    let viewless = view.is_none_or(|v| v.dst == b && v.src.loc == (0.0, 0.0).into() && v.src.size == b.to_f64());
    transform == Transform::Normal && viewless
}

/// The size `w` x `h` becomes at factor `f`, None when the hardware cannot show it.
pub fn fits(w: u32, h: u32, f: f64) -> Option<(u32, u32)> {
    let (w2, h2) = ((w as f64 * f).round(), (h as f64 * f).round());
    (w > 0 && h > 0 && w2 >= 1.0 && h2 >= 1.0 && w2 <= HW_MAX as f64 && h2 <= HW_MAX as f64).then_some((w2 as u32, h2 as u32))
}

/// A client's cursor surface as an image at the output scale `s` (its buffer scale undone), hotspot in pixels.
pub fn image(surface: &WlSurface, s: f64) -> Want {
    if !get_children(surface).is_empty() {
        return Want::Software;
    }
    let Some((buf, bscale, ok)) = with_renderer_surface_state(surface, |st| {
        (st.buffer().map(|b| (**b).clone()), st.buffer_scale(), plain(st.buffer_transform(), st.view(), st.buffer_size()))
    }) else {
        return Want::Software;
    };
    let Some(buf) = buf else { return Want::Nothing };
    if !ok {
        return Want::Software;
    }
    let hot = hotspot(surface);
    let f = s / bscale.max(1) as f64;
    let read = with_buffer_contents(&buf, |ptr, len, d| {
        let (w, h, stride, off) = (d.width.max(0) as usize, d.height.max(0) as usize, d.stride.max(0) as usize, d.offset.max(0) as usize);
        let opaque = match d.format {
            Format::Argb8888 => false,
            Format::Xrgb8888 => true,
            _ => return None,
        };
        // Before any copy: what the hardware cannot show is not read at all.
        fits(w as u32, h as u32, f)?;
        if stride < w * 4 || off.checked_add(stride.checked_mul(h)?)? > len {
            return None;
        }
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                // SAFETY: off + y*stride + x*4 + 4 <= off + stride*h <= len (checked above), inside the pool the
                // callback lends us; read_unaligned makes no reference into memory the client may change.
                let p = unsafe { std::ptr::read_unaligned(ptr.add(off + y * stride + x * 4).cast::<u32>()) };
                px.push(if opaque { p | 0xff00_0000 } else { p });
            }
        }
        Some((w as u32, h as u32, px))
    });
    let Ok(Some((w, h, px))) = read else { return Want::Software };
    let (w2, h2, px) = if (f - 1.0).abs() < 1e-6 { (w, h, px) } else { scale_nearest(&px, w, h, f) };
    let hx = ((hot.0 as f64 * s).round() as i64).clamp(0, w2 as i64 - 1) as u32;
    let hy = ((hot.1 as f64 * s).round() as i64).clamp(0, h2 as i64 - 1) as u32;
    Want::Image(Rc::new(CursorImage { w: w2, h: h2, hot: (hx, hy), px, what: "surface".into() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smithay::utils::{Point, Rectangle};

    #[test]
    fn what_the_hardware_can_show() {
        assert_eq!(fits(32, 32, 1.25), Some((40, 40)));
        assert_eq!(fits(256, 256, 1.0), Some((256, 256)));
        assert_eq!(fits(205, 10, 1.25), Some((256, 13)));
        assert_eq!(fits(206, 10, 1.25), None);
        assert_eq!(fits(300, 300, 1.0), None);
        assert_eq!(fits(0, 10, 1.0), None);
        assert_eq!(fits(u32::MAX, u32::MAX, 1.0), None);
    }

    #[test]
    fn only_plain_surfaces() {
        let b: Size<i32, Logical> = (32, 32).into();
        let full = SurfaceView { src: Rectangle::from_size(b.to_f64()), dst: b, offset: Point::from((0, 0)) };
        assert!(plain(Transform::Normal, None, Some(b)));
        assert!(plain(Transform::Normal, Some(full), Some(b)));
        assert!(!plain(Transform::_90, Some(full), Some(b)));
        assert!(!plain(Transform::Normal, Some(SurfaceView { dst: (64, 64).into(), ..full }), Some(b)));
        let crop = SurfaceView { src: Rectangle::new((4.0, 0.0).into(), (28.0, 32.0).into()), ..full };
        assert!(!plain(Transform::Normal, Some(crop), Some(b)));
        assert!(!plain(Transform::Normal, None, None));
    }
}
