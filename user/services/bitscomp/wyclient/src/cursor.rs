//! WY-11 `--cursor NAME|hidden|surface`: on every pointer enter the client sets its pointer as a browser does:
//! a named shape through wp_cursor_shape_v1 (default, text, pointer, wait, progress, crosshair, grab, ew-resize,
//! not-allowed), none (`hidden`: wl_pointer.set_cursor without a surface), or its own 32x32 cursor surface
//! (`surface`: a magenta square with a white top-left pixel at the hotspot 3,5; buffer scale 1; every second enter
//! sets the same surface again with the hotspot 4,6 and no new commit, as toolkits re-send set_cursor; `big`: a
//! 300x300 one, more than a hardware cursor takes). Prints `cursor set NAME HX,HY` (or `cursor no shape manager`).

use std::os::fd::{AsFd, AsRawFd};

use wayland_client::protocol::{wl_pointer, wl_shm, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;

use super::State;

pub const SIDE: i32 = 32;
pub const BIG: i32 = 300;
pub const HOT: (i32, i32) = (3, 5);

static ENTERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

pub fn shape(name: &str) -> Option<Shape> {
    Some(match name {
        "default" => Shape::Default,
        "text" => Shape::Text,
        "pointer" => Shape::Pointer,
        "wait" => Shape::Wait,
        "progress" => Shape::Progress,
        "crosshair" => Shape::Crosshair,
        "grab" => Shape::Grab,
        "ew-resize" => Shape::EwResize,
        "not-allowed" => Shape::NotAllowed,
        _ => return None,
    })
}

/// The pointer entered our surface with `serial`: set the pointer the arguments asked for.
pub fn on_enter(st: &mut State, p: &wl_pointer::WlPointer, serial: u32, qh: &QueueHandle<State>) {
    let Some(name) = st.cursor.clone() else { return };
    let k = ENTERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) as i32 % 2;
    let hot = (HOT.0 + k, HOT.1 + k);
    match name.as_str() {
        "hidden" => p.set_cursor(serial, None, 0, 0),
        "surface" | "big" => match surface(st, qh, if name == "big" { BIG } else { SIDE }) {
            Ok(s) => p.set_cursor(serial, Some(&s), hot.0, hot.1),
            Err(e) => {
                println!("cursor surface failed: {e}");
                return;
            }
        },
        n => {
            let (Some(m), Some(sh)) = (st.shape_mgr.as_ref(), shape(n)) else {
                println!("cursor no shape manager");
                return;
            };
            let dev = m.get_pointer(p, qh, ());
            dev.set_shape(serial, sh);
            dev.destroy();
        }
    }
    println!("cursor set {name} {},{}", hot.0, hot.1);
}

fn surface(st: &mut State, qh: &QueueHandle<State>, side: i32) -> std::io::Result<wl_surface::WlSurface> {
    if let Some(s) = &st.cursor_surface {
        return Ok(s.clone());
    }
    let comp = st.compositor.as_ref().ok_or_else(|| std::io::Error::other("no wl_compositor"))?;
    let shm = st.shm.as_ref().ok_or_else(|| std::io::Error::other("no wl_shm"))?;
    let len = (side * side * 4) as usize;
    let fd = super::memfd(len)?;
    let mut px = vec![0xffff_00ffu32; (side * side) as usize];
    px[(HOT.1 * side + HOT.0) as usize] = 0xffff_ffff;
    let bytes: Vec<u8> = px.iter().flat_map(|p| p.to_le_bytes()).collect();
    // SAFETY: pwrite from our buffer into our own memfd.
    let n = unsafe { libc::pwrite(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), 0) };
    if n != bytes.len() as isize {
        return Err(std::io::Error::other(format!("pwrite {n} of {}", bytes.len())));
    }
    let pool = shm.create_pool(fd.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(0, side, side, side * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    let s = comp.create_surface(qh, ());
    s.attach(Some(&buffer), 0, 0);
    s.damage_buffer(0, 0, side, side);
    s.commit();
    st.cursor_surface = Some(s.clone());
    Ok(s)
}

impl Dispatch<WpCursorShapeManagerV1, ()> for State {
    fn event(_: &mut Self, _: &WpCursorShapeManagerV1, _: <WpCursorShapeManagerV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for State {
    fn event(_: &mut Self, _: &WpCursorShapeDeviceV1, _: <WpCursorShapeDeviceV1 as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
