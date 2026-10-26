//! WY-5b: `bitscomp page ADDRESS`, the window of a settings page. bitscomp draws the page itself (`page_draw`,
//! at the output's scale, from its own theme and settings values); this small client only gives the page a real
//! xdg toplevel, so tabs, tiles, workspaces, the session and the window list treat it like any other window. It
//! maps a 1x1 buffer (the compositor paints over the whole content), answers pings and ends on close or when
//! the connection goes. Pure Rust wayland-client (no libwayland on V2), as wyclient. The window still honours
//! each configured size (xdg-shell): wp_viewporter stretches the one pixel to it and the window geometry says
//! it, so every size, hit box and capture of the window is the tile's and no tile-sized buffer is needed.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use wayland_client::protocol::{wl_buffer, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

/// The app id bitscomp knows its pages by (with the pid of a page it started).
pub const APP_ID: &str = "bitscomp.einstellungen";
pub const TITLE: &str = "Einstellungen";

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm: Option<xdg_wm_base::XdgWmBase>,
    surface: Option<wl_surface::WlSurface>,
    buffer: Option<wl_buffer::WlBuffer>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    viewport: Option<wp_viewport::WpViewport>,
    xdg: Option<xdg_surface::XdgSurface>,
    /// The size of the last toplevel configure that named one (0x0 = "you choose": the last size stays).
    asked: (i32, i32),
    done: bool,
}

fn memfd() -> std::io::Result<OwnedFd> {
    // SAFETY: memfd_create with a NUL-terminated name; the fd is checked and owned below.
    let fd = unsafe { libc::memfd_create(c"bitscomp-page".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` was just made and is owned by nobody else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: ftruncate on our own fd to one XRGB pixel.
    if unsafe { libc::ftruncate(fd.as_raw_fd(), 4) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fd)
}

impl State {
    fn map(&mut self, qh: &QueueHandle<State>) -> std::io::Result<()> {
        let fd = memfd()?;
        let shm = self.shm.as_ref().ok_or_else(|| std::io::Error::other("no wl_shm"))?;
        let pool = shm.create_pool(fd.as_fd(), 4, qh, ());
        let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Xrgb8888, qh, ());
        pool.destroy();
        let s = self.surface.as_ref().ok_or_else(|| std::io::Error::other("no surface"))?;
        s.attach(Some(&buffer), 0, 0);
        s.damage_buffer(0, 0, 1, 1);
        self.buffer = Some(buffer);
        Ok(())
    }

    /// The configured size as the window's size (before the commit that acks it).
    fn size(&self) {
        let (w, h) = self.asked;
        if w <= 0 || h <= 0 {
            return;
        }
        if let Some(v) = &self.viewport {
            v.set_destination(w, h);
        }
        if let Some(x) = &self.xdg {
            x.set_window_geometry(0, 0, w, h);
        }
    }
}

/// Runs the page window for `address` until it is closed; the exit code for main.
pub fn run(address: &str) -> i32 {
    match serve(address) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("bitscomp page {address}: {e}");
            1
        }
    }
}

fn serve(address: &str) -> Result<(), String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("no Wayland display: {e}"))?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut st = State::default();
    queue.roundtrip(&mut st).map_err(|e| e.to_string())?;
    let (Some(comp), Some(wm)) = (st.compositor.clone(), st.wm.clone()) else {
        return Err("the compositor lacks wl_compositor or xdg_wm_base".into());
    };
    if st.shm.is_none() {
        return Err("the compositor lacks wl_shm".into());
    }
    let surface = comp.create_surface(&qh, ());
    let xdg = wm.get_xdg_surface(&surface, &qh, ());
    let top = xdg.get_toplevel(&qh, ());
    st.viewport = st.viewporter.as_ref().map(|v| v.get_viewport(&surface, &qh, ()));
    if st.viewport.is_none() {
        eprintln!("bitscomp page {address}: no wp_viewporter: the buffer stays 1x1 (window geometry only)");
    }
    st.xdg = Some(xdg);
    top.set_app_id(APP_ID.into());
    top.set_title(TITLE.into());
    surface.commit();
    st.surface = Some(surface);
    // The address is in our argv (bitscomp reads it from what it started); printed for the log only.
    eprintln!("bitscomp page {address}: mapped");
    while !st.done {
        queue.blocking_dispatch(&mut st).map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(st: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "wl_compositor" => st.compositor = Some(reg.bind(name, version.min(4), qh, ())),
                "wl_shm" => st.shm = Some(reg.bind(name, 1, qh, ())),
                "xdg_wm_base" => st.wm = Some(reg.bind(name, version.min(2), qh, ())),
                "wp_viewporter" => st.viewporter = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, wm: &xdg_wm_base::XdgWmBase, ev: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = ev {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(st: &mut Self, x: &xdg_surface::XdgSurface, ev: xdg_surface::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = ev {
            x.ack_configure(serial);
            if st.buffer.is_none()
                && let Err(e) = st.map(qh)
            {
                eprintln!("bitscomp page: buffer: {e}");
                st.done = true;
                return;
            }
            st.size();
            if let Some(s) = &st.surface {
                s.commit();
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(st: &mut Self, _: &xdg_toplevel::XdgToplevel, ev: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            xdg_toplevel::Event::Close => st.done = true,
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => st.asked = (width, height),
            _ => {}
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
