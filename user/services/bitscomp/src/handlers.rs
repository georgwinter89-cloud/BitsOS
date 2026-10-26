//! Protocol handlers. Window management decisions live in `wm.rs`; these only translate protocol events.

use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::desktop::{PopupKind, Window, find_popup_root_surface, get_popup_toplevel_coords};
use smithay::input::pointer::CursorImageStatus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecoMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface};
use smithay::utils::Serial;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    CompositorClientState, CompositorHandler, CompositorState, get_parent, is_sync_subsurface, with_states,
};
use smithay::wayland::fractional_scale::{FractionalScaleHandler, with_fractional_scale};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler, set_data_device_focus,
};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ShellClient, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_fractional_scale, delegate_output, delegate_presentation,
    delegate_seat, delegate_shm, delegate_viewporter, delegate_xdg_decoration, delegate_xdg_shell,
};

use crate::ClientState;
use crate::state::App;

impl CompositorHandler for App {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.needs_frame = true;
        self.stats.commits += 1;
        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }
            if let Some(window) = self.find_window(&root) {
                window.on_commit();
                if self.note_first_frame(&window, surface) {
                    // WY-5b: a foot of ours has its signal handlers now.
                    if let Ok(pid) = u32::try_from(self.pid_of(&window)) {
                        self.foot_drawn(pid);
                        // WY-7: its start (or its launcher's: the same process group) is answered now.
                        // SAFETY: getpgid only reads the pid; an ended or foreign pid answers -1 (ESRCH).
                        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
                        let group = u32::try_from(pgid).unwrap_or(pid);
                        if self.launches.mapped(pid) | self.launches.mapped(group) {
                            self.needs_frame = true;
                        }
                    }
                    // WY-5: the start page's "recently used" (the app id is known by the first frame).
                    let id = crate::ctl::names(&window).0;
                    if !id.is_empty() {
                        let a = self.address_of(&window);
                        self.recent_add(&a);
                    }
                }
                let toplevel = window.toplevel().unwrap();
                if !toplevel.is_initial_configure_sent() {
                    // The size and server-side frame were set as pending state when the window was placed.
                    toplevel.send_configure();
                }
            }
        }
        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(xdg)) = self.popups.find_popup(surface)
            && !xdg.is_initial_configure_sent()
        {
            let _ = xdg.send_configure();
        }
    }
}

/// A request that changed nothing is still answered with the state as it is, so the client is not left waiting.
/// A hung client gets none (B4: configures would only pile up in its socket); it gets the pending state once it
/// answers again.
fn answer(app: &App, surface: &ToplevelSurface, sent: bool) {
    if sent || !surface.is_initial_configure_sent() {
        return;
    }
    if app.find_window(surface.wl_surface()).is_some_and(|w| app.is_hung(&w)) {
        return;
    }
    surface.send_configure();
}

struct FirstFrameSeen;

impl App {
    /// B6: a window's first frame, once, with its program's pid and, for a program bitscomp started, the time
    /// since the spawn (WY-1b: foot's startup time on V2).
    fn note_first_frame(&self, window: &Window, surface: &WlSurface) -> bool {
        let has_buffer = smithay::backend::renderer::utils::with_renderer_surface_state(surface, |s| s.buffer().is_some())
            .unwrap_or(false);
        if !has_buffer || window.user_data().get::<FirstFrameSeen>().is_some() {
            return false;
        }
        window.user_data().insert_if_missing(|| FirstFrameSeen);
        // SO_PEERCRED, read once at accept (`clients::peer_pid`).
        let pid = Some(self.pid_of(window) as u32).filter(|&p| p > 0);
        let since = pid.and_then(|p| self.children.iter().find(|(c, _)| c.id() == p)).map(|(_, t)| t.elapsed());
        let (app_id, _) = crate::ctl::names(window);
        match (pid, since) {
            (Some(p), Some(d)) => eprintln!(
                "bitscomp: window {} ({app_id}) of pid {p}: first frame {} ms after its start",
                crate::ctl::win_id(window),
                d.as_millis()
            ),
            (p, _) => eprintln!("bitscomp: window {} ({app_id}) of pid {}: first frame", crate::ctl::win_id(window), p.unwrap_or(0)),
        }
        true
    }
}

impl BufferHandler for App {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for App {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for App {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        let window = Window::new_wayland_window(surface);
        crate::ctl::win_id(&window);
        self.place_new(window);
        self.layout_dirty = true;
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(w) = self.find_window(surface.wl_surface()) {
            self.forget_window(&w);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(&mut self, surface: PopupSurface, positioner: PositionerState, token: u32) {
        surface.with_pending_state(|s| {
            s.geometry = positioner.get_geometry();
            s.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        // Popup grabs (menus closing on outside clicks) are not needed to prove the skeleton; WY-1.
    }

    fn move_request(&mut self, surface: ToplevelSurface, _seat: wl_seat::WlSeat, _serial: Serial) {
        if let Some(w) = self.find_window(surface.wl_surface()) {
            self.begin_move(&w);
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // WY-5c/WY-8: a window may ask to be maximised (its own button, a double click on its own tab strip), as
        // Ctrl+Alt+M does it. Always answered, so the client is not left waiting.
        let sent = self.client_max(&surface, true);
        answer(self, &surface, sent);
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        let sent = self.client_max(&surface, false);
        answer(self, &surface, sent);
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<wl_output::WlOutput>) {
        // WY-8: there is one output; whichever the client names, it is that one.
        let sent = self.client_fullscreen(&surface, true);
        answer(self, &surface, sent);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        let sent = self.client_fullscreen(&surface, false);
        answer(self, &surface, sent);
    }

    /// WY-8: a dialog of the shown fullscreen window (its parent, set before the dialog's first commit) is
    /// focused, which ends the fullscreen (sway's `popup_during_fullscreen leave_fullscreen`): bitscomp has no
    /// stacking over a fullscreen window, and a dialog hidden under it would wait unseen.
    fn parent_changed(&mut self, surface: ToplevelSurface) {
        if surface.is_initial_configure_sent() {
            return;
        }
        let (Some(parent), Some(child)) = (surface.parent(), self.find_window(surface.wl_surface())) else { return };
        let Some(fs) = self.ws.fullscreen_shown().cloned() else { return };
        if fs.toplevel().is_some_and(|t| *t.wl_surface() == parent) && self.ws.find(&child).is_some() {
            eprintln!("bitscomp: window {} opens a dialog over its fullscreen", crate::ctl::win_id(&fs));
            self.ws.focus_window(&child);
            self.layout_dirty = true;
        }
    }

    fn title_changed(&mut self, _surface: ToplevelSurface) {
        self.needs_frame = true;
    }

    fn client_pong(&mut self, client: ShellClient) {
        self.pong(&client);
    }
}

impl XdgDecorationHandler for App {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecoMode) {
        // BitsOS draws every frame itself (§8a.2), whatever the client asks for.
        toplevel.with_pending_state(|s| s.decoration_mode = Some(DecoMode::ServerSide));
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.request_mode(toplevel, DecoMode::ServerSide);
    }
}

impl SeatHandler for App {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<App> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let client = focused.and_then(|s| self.dh.get_client(s.id()).ok());
        let pid = client.as_ref().and_then(|c| c.get_data::<ClientState>().map(|d| d.pid)).unwrap_or(0);
        if self.grant.as_mut().is_some_and(|g| g.focus(pid).is_err()) {
            self.grant = None;
        }
        set_data_device_focus(&self.dh, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_named = match &image {
            CursorImageStatus::Named(icon) => Some(*icon),
            _ => None,
        };
        self.cursor_hidden = image == CursorImageStatus::Hidden;
        self.cursor_surface = match image {
            CursorImageStatus::Surface(s) => Some(s),
            _ => None,
        };
        if self.draw_pointer {
            self.needs_frame = true;
        }
        // WY-11: a new shape reaches the hardware cursor at once (not at the next loop turn).
        self.cursor_sync();
    }
}

// WY-11: wp_cursor_shape_v1 also serves tablet tools; bitscomp has none (the trait's defaults).
impl smithay::wayland::tablet_manager::TabletSeatHandler for App {}

impl SelectionHandler for App {
    type SelectionUserData = ();
}

impl DataDeviceHandler for App {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for App {}
impl ServerDndGrabHandler for App {}

impl OutputHandler for App {}

impl FractionalScaleHandler for App {
    fn new_fractional_scale(&mut self, surface: WlSurface) {
        let scale = self.theme.scale;
        with_states(&surface, |states| with_fractional_scale(states, |fs| fs.set_preferred_scale(scale)));
    }
}

impl App {
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else { return };
        let Some(window) = self.space.elements().find(|w| w.toplevel().is_some_and(|t| t.wl_surface() == &root)) else {
            return;
        };
        let Some(win_loc) = self.space.element_location(window) else { return };
        let Some(mut target) = self.space.output_geometry(&self.output) else { return };
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        target.loc -= win_loc;
        popup.with_pending_state(|s| s.geometry = s.positioner.get_unconstrained_geometry(target));
    }

    /// A window's own (un)maximise request; returns whether a configure went out. A tile floats first, as with
    /// Ctrl+Alt+M. Before its first configure the request is a remembered state (a browser's last session), not
    /// a wish of the user's: the window takes its place in the tiles as any other.
    fn client_max(&mut self, surface: &ToplevelSurface, max: bool) -> bool {
        let Some(w) = self.find_window(surface.wl_surface()) else { return false };
        if !surface.is_initial_configure_sent() || self.ws.find(&w).is_none() || self.ws.is_max(&w) == max {
            return false;
        }
        if self.ws.is_fullscreen(&w) {
            // Remembered for the way back would need a third state; the fullscreen stays as it is.
            return false;
        }
        if matches!(self.ws.find(&w), Some(crate::layout::Place::Tiled { .. })) {
            self.ws.toggle_float(&w, self.area());
        }
        self.ws.toggle_max(&w, self.area_full());
        eprintln!("bitscomp: window {} {} on its own request", crate::ctl::win_id(&w), if max { "maximised" } else { "restored" });
        self.configured_by_layout(&w)
    }

    /// WY-8: a window's own fullscreen request (also before its first configure: that one then is fullscreen
    /// already); returns whether a configure went out. A window of a parked workspace gets its state now and
    /// covers the screen only once its workspace is shown.
    fn client_fullscreen(&mut self, surface: &ToplevelSurface, on: bool) -> bool {
        let Some(w) = self.find_window(surface.wl_surface()) else { return false };
        let id = crate::ctl::win_id(&w);
        if on && self.is_hung(&w) {
            // Not answering pings but asking for the screen: refused, and no configure (B4).
            eprintln!("bitscomp: window {id} asks for fullscreen while it does not answer: refused");
            return false;
        }
        if self.ws.find(&w).is_some() {
            if !self.ws.set_fullscreen(&w, on) {
                if on && self.ws.focused() != Some(&w) {
                    eprintln!("bitscomp: window {id} asks for fullscreen without the focus: refused");
                }
                return false;
            }
            eprintln!("bitscomp: window {id} {} fullscreen on its own request", if on { "enters" } else { "leaves" });
            if on {
                // Keys go to the client now, and no bar comes over it.
                self.address_end();
                self.page_edit_end();
                if self.desk.reveal.clear() {
                    self.chrome_dirty();
                }
            }
            return self.configured_by_layout(&w);
        }
        let (area, full, gap) = (self.area(), self.area_full(), self.theme.gap);
        let Some(ws) = self.desk.parked.iter_mut().find(|ws| ws.find(&w).is_some()) else { return false };
        if !ws.set_fullscreen(&w, on) {
            return false;
        }
        eprintln!("bitscomp: window {id} {} fullscreen on a parked workspace", if on { "enters" } else { "leaves" });
        let arranged = ws.arrange(area, full, gap);
        let placed = self.fit(arranged);
        placed.iter().find(|p| p.win == w).is_some_and(|p| self.configure(p))
    }

    /// Lays out at once and says whether `w` got its configure from that.
    fn configured_by_layout(&mut self, w: &Window) -> bool {
        self.apply_layout();
        let t = w.toplevel().unwrap();
        t.is_initial_configure_sent() && !self.is_hung(w)
    }

    /// `maximised` (WY-5c): the xdg `maximized` state of a maximised float, so the client shows its own restore
    /// glyph and stops asking for a size it already has (xdg-shell: the configured size must then be obeyed).
    /// `fullscreen` (WY-8): the client covers the screen and draws no frame of its own.
    pub fn tiled_states(toplevel: &ToplevelSurface, tiled: bool, maximised: bool, activated: bool, fullscreen: bool) {
        toplevel.with_pending_state(|s| {
            if fullscreen {
                s.states.set(xdg_toplevel::State::Fullscreen);
            } else {
                s.states.unset(xdg_toplevel::State::Fullscreen);
            }
            if maximised {
                s.states.set(xdg_toplevel::State::Maximized);
            } else {
                s.states.unset(xdg_toplevel::State::Maximized);
            }
            for st in [
                xdg_toplevel::State::TiledLeft,
                xdg_toplevel::State::TiledRight,
                xdg_toplevel::State::TiledTop,
                xdg_toplevel::State::TiledBottom,
            ] {
                if tiled {
                    s.states.set(st);
                } else {
                    s.states.unset(st);
                }
            }
            if activated {
                s.states.set(xdg_toplevel::State::Activated);
            } else {
                s.states.unset(xdg_toplevel::State::Activated);
            }
        });
    }
}

delegate_compositor!(App);
delegate_shm!(App);
delegate_xdg_shell!(App);
delegate_xdg_decoration!(App);
delegate_seat!(App);
delegate_data_device!(App);
delegate_output!(App);
delegate_viewporter!(App);
delegate_fractional_scale!(App);
delegate_presentation!(App);
smithay::delegate_cursor_shape!(App);
