//! WY-3: bitscomp's window: an xdg toplevel (Smithay's `Window`) or an X11 window of Xwayland. Smithay's own
//! X11 variant needs its `xwayland` feature and with it x11rb, which V2 may not download; so the X11 side is
//! bitscomp's (`xwm`) and this type carries both with Smithay's method names, so the window code stays one.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use smithay::backend::renderer::element::surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree};
use smithay::backend::renderer::element::{AsRenderElements, Kind};
use smithay::backend::renderer::{ImportAll, Renderer};
use smithay::desktop::space::SpaceElement;
use smithay::desktop::utils::{
    OutputPresentationFeedback, bbox_from_surface_tree, send_frames_surface_tree,
    take_presentation_feedback_surface_tree, under_from_surface_tree, with_surfaces_surface_tree,
};
use smithay::desktop::WindowSurfaceType;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::ToplevelSurface;

/// What bitscomp knows of one X11 top level, shared between the window list and the WM.
#[derive(Debug, Default)]
pub struct XState {
    pub xid: u32,
    pub surface: RefCell<Option<WlSurface>>,
    /// Where the X server has the window (root coordinates = bitscomp's logical coordinates).
    pub geo: Cell<Rectangle<i32, Logical>>,
    pub title: RefCell<String>,
    /// WM_CLASS's class part (the app id of an X11 program).
    pub class: RefCell<String>,
    /// The pid the X server knows from the connection's peer credentials (X-Resource); 0 when unknown.
    pub pid: Cell<u32>,
    /// The pid the window claims in _NET_WM_PID: any X client can write it, so it is only shown in log lines,
    /// never used to match launches, restart a session or answer `windows`.
    pub claimed_pid: Cell<u32>,
    pub override_redirect: bool,
    /// WM_DELETE_WINDOW in WM_PROTOCOLS (else a close kills the client).
    pub deletes: Cell<bool>,
    pub takes_focus: Cell<bool>,
    /// WM_PROTOCOLS was read (or is known to be missing): until then a close waits, so a program that supports
    /// WM_DELETE_WINDOW is never killed only because its answer was late.
    pub protocols_read: Cell<bool>,
    /// WM_HINTS says input=False (ICCCM 4.1.7: the WM never sets the focus on it; WM_TAKE_FOCUS only).
    pub no_input: Cell<bool>,
    pub transient_for: Cell<u32>,
    /// `_NET_WM_STATE_FULLSCREEN` asked for before the window showed.
    pub wants_full: Cell<bool>,
    /// The fullscreen state the window was last told in `_NET_WM_STATE`.
    pub full_told: Cell<bool>,
    /// What bitscomp asked for and the WM has not sent yet: the content rectangle, a close.
    pub want_geo: Cell<Option<Rectangle<i32, Logical>>>,
    pub want_close: Cell<bool>,
    pub want_kill: Cell<bool>,
    pub activated: Cell<bool>,
    pub alive: Cell<bool>,
    entered: RefCell<Vec<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>>,
    user_data: UserDataMap,
}

impl XState {
    pub fn new(xid: u32, override_redirect: bool, geo: Rectangle<i32, Logical>) -> Self {
        let s = XState { xid, override_redirect, ..Default::default() };
        s.geo.set(geo);
        s.alive.set(true);
        s
    }

    pub fn wl_surface(&self) -> Option<WlSurface> {
        self.surface.borrow().clone()
    }
}

#[derive(Debug, Clone)]
pub enum Window {
    Wl(smithay::desktop::Window),
    X(Rc<XState>),
}

impl PartialEq for Window {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Window::Wl(a), Window::Wl(b)) => a == b,
            (Window::X(a), Window::X(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl Eq for Window {}

impl std::hash::Hash for Window {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        match self {
            Window::Wl(w) => w.hash(h),
            Window::X(x) => (Rc::as_ptr(x) as usize).hash(h),
        }
    }
}

impl Window {
    pub fn new_wayland_window(t: ToplevelSurface) -> Self {
        Window::Wl(smithay::desktop::Window::new_wayland_window(t))
    }

    pub fn toplevel(&self) -> Option<&ToplevelSurface> {
        match self {
            Window::Wl(w) => w.toplevel(),
            Window::X(_) => None,
        }
    }

    pub fn x11(&self) -> Option<&Rc<XState>> {
        match self {
            Window::X(x) => Some(x),
            Window::Wl(_) => None,
        }
    }

    /// The window's own surface (an X11 window has one once Xwayland named it).
    pub fn surface(&self) -> Option<WlSurface> {
        match self {
            Window::Wl(w) => w.toplevel().map(|t| t.wl_surface().clone()),
            Window::X(x) => x.wl_surface(),
        }
    }

    pub fn user_data(&self) -> &UserDataMap {
        match self {
            Window::Wl(w) => w.user_data(),
            Window::X(x) => &x.user_data,
        }
    }

    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        match self {
            Window::Wl(w) => w.geometry(),
            Window::X(x) => Rectangle::from_size(x.geo.get().size),
        }
    }

    pub fn bbox(&self) -> Rectangle<i32, Logical> {
        match self {
            Window::Wl(w) => w.bbox(),
            Window::X(x) => match x.wl_surface() {
                Some(s) => bbox_from_surface_tree(&s, (0, 0)),
                None => Rectangle::from_size(x.geo.get().size),
            },
        }
    }

    pub fn set_activated(&self, active: bool) -> bool {
        match self {
            Window::Wl(w) => w.set_activated(active),
            Window::X(x) => x.activated.replace(active) != active,
        }
    }

    pub fn on_commit(&self) {
        if let Window::Wl(w) = self {
            w.on_commit();
        }
    }

    pub fn send_frame<T, F>(&self, output: &Output, time: T, throttle: Option<Duration>, primary: F)
    where
        T: Into<Duration>,
        F: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
    {
        match self {
            Window::Wl(w) => w.send_frame(output, time, throttle, primary),
            Window::X(x) => {
                if let Some(s) = x.wl_surface() {
                    send_frames_surface_tree(&s, output, time.into(), throttle, primary);
                }
            }
        }
    }

    pub fn take_presentation_feedback<F1, F2>(&self, feedback: &mut OutputPresentationFeedback, primary: F1, flags: F2)
    where
        F1: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
        F2: FnMut(&WlSurface, &SurfaceData) -> wp_presentation_feedback::Kind + Copy,
    {
        match self {
            Window::Wl(w) => w.take_presentation_feedback(feedback, primary, flags),
            Window::X(x) => {
                if let Some(s) = x.wl_surface() {
                    take_presentation_feedback_surface_tree(&s, feedback, primary, flags);
                }
            }
        }
    }

    pub fn with_surfaces<F: FnMut(&WlSurface, &SurfaceData)>(&self, f: F) {
        match self {
            Window::Wl(w) => w.with_surfaces(f),
            Window::X(x) => {
                if let Some(s) = x.wl_surface() {
                    with_surfaces_surface_tree(&s, f);
                }
            }
        }
    }

    pub fn surface_under<P: Into<Point<f64, Logical>>>(&self, p: P, ty: WindowSurfaceType) -> Option<(WlSurface, Point<i32, Logical>)> {
        match self {
            Window::Wl(w) => w.surface_under(p, ty),
            Window::X(x) => under_from_surface_tree(&x.wl_surface()?, p.into(), (0, 0), ty),
        }
    }
}

impl WaylandFocus for Window {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        self.surface().map(Cow::Owned)
    }
}

impl IsAlive for Window {
    fn alive(&self) -> bool {
        match self {
            Window::Wl(w) => w.alive(),
            Window::X(x) => x.alive.get() && x.wl_surface().is_none_or(|s| s.alive()),
        }
    }
}

impl SpaceElement for Window {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        Window::geometry(self)
    }
    fn bbox(&self) -> Rectangle<i32, Logical> {
        Window::bbox(self)
    }
    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        match self {
            Window::Wl(w) => w.is_in_input_region(point),
            Window::X(_) => self.surface_under(*point, WindowSurfaceType::ALL).is_some(),
        }
    }
    fn z_index(&self) -> u8 {
        match self {
            Window::Wl(w) => w.z_index(),
            Window::X(_) => smithay::desktop::space::RenderZindex::Shell as u8,
        }
    }
    fn set_activate(&self, activated: bool) {
        match self {
            Window::Wl(w) => w.set_activate(activated),
            Window::X(_) => {
                self.set_activated(activated);
            }
        }
    }
    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        match self {
            Window::Wl(w) => w.output_enter(output, overlap),
            // wl_surface.enter once per output of Xwayland's (its scale and output follow from it).
            Window::X(x) => {
                let (Some(s), mut entered) = (x.wl_surface(), x.entered.borrow_mut()) else { return };
                let Some(client) = s.client() else { return };
                for o in output.client_outputs(&client) {
                    if !entered.contains(&o) {
                        s.enter(&o);
                        entered.push(o);
                    }
                }
            }
        }
    }
    fn output_leave(&self, output: &Output) {
        match self {
            Window::Wl(w) => w.output_leave(output),
            Window::X(x) => {
                let (Some(s), mut entered) = (x.wl_surface(), x.entered.borrow_mut()) else { return };
                let Some(client) = s.client() else { return };
                for o in output.client_outputs(&client) {
                    if let Some(i) = entered.iter().position(|e| *e == o) {
                        s.leave(&o);
                        entered.remove(i);
                    }
                }
            }
        }
    }
    fn refresh(&self) {
        if let Window::Wl(w) = self {
            w.refresh();
        }
    }
}

impl<R> AsRenderElements<R> for Window
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    type RenderElement = WaylandSurfaceRenderElement<R>;

    fn render_elements<C: From<WaylandSurfaceRenderElement<R>>>(
        &self,
        renderer: &mut R,
        location: Point<i32, Physical>,
        scale: Scale<f64>,
        alpha: f32,
    ) -> Vec<C> {
        match self {
            Window::Wl(w) => w.render_elements(renderer, location, scale, alpha),
            Window::X(x) => match x.wl_surface() {
                Some(s) => render_elements_from_surface_tree(renderer, &s, location, scale, alpha, Kind::Unspecified),
                None => Vec::new(),
            },
        }
    }
}
