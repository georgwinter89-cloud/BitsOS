//! WY-3: bitscomp's X11 window manager on Xwayland's `-wm` connection. It redirects the top levels (Composite,
//! so Xwayland gives each its own wl_surface), maps what clients ask to map, learns each window's surface from
//! Xwayland's WL_SURFACE_ID message (no xwayland_shell_v1 is offered, so Xwayland 24.1 sends that one), and hands
//! the window to the desk as any other window: placed, framed and focused by bitscomp. Override-redirect windows
//! (menus, tooltips) are shown where the client put them, over everything. X root coordinates are bitscomp's
//! logical coordinates (Xwayland sizes its root from xdg-output's logical size).

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::utils::{Logical, Rectangle};

use crate::state::App;
use crate::win::{Window, XState};
use crate::xconn::{Msg, XConn};
use crate::xproto::{self as xp, i16_at, u16_at, u32_at};

macro_rules! atoms {
    ($($name:ident),* $(,)?) => {
        #[allow(non_snake_case)]
        #[derive(Debug, Default, Clone)]
        pub struct Atoms { $(pub $name: u32,)* }
        impl Atoms {
            const NAMES: &'static [&'static str] = &[$(stringify!($name)),*];
            fn set(&mut self, i: usize, v: u32) {
                let mut n = 0;
                $( if n == i { self.$name = v; } n += 1; )*
                let _ = n;
            }
        }
    };
}

atoms!(
    WM_PROTOCOLS, WM_DELETE_WINDOW, WM_TAKE_FOCUS, WM_STATE, WL_SURFACE_ID, UTF8_STRING, CLIPBOARD, TARGETS,
    TIMESTAMP, INCR, TEXT, _NET_WM_NAME, _NET_WM_PID, _NET_SUPPORTED, _NET_SUPPORTING_WM_CHECK, _NET_ACTIVE_WINDOW,
    _BITSCOMP_SELECTION, _BITSCOMP_TIME, MIME_TEXT_UTF8, MIME_TEXT, WM_S0,
);

/// The atom name of a field whose Rust name cannot carry it.
fn atom_name(n: &str) -> &str {
    match n {
        "MIME_TEXT_UTF8" => "text/plain;charset=utf-8",
        "MIME_TEXT" => "text/plain",
        other => other,
    }
}

#[derive(Debug)]
pub enum Want {
    Atom(usize),
    Ext(&'static str),
    Prop(u32, u32),
    Pid(u32),
    Sel(crate::xsel::Want),
    Ignore,
}

pub struct Xwm {
    pub conn: XConn<Want>,
    pub atoms: Atoms,
    pub root: u32,
    pub wm_win: u32,
    pub ready: bool,
    left: usize,
    composite: u8,
    xfixes: u8,
    pub xfixes_event: u8,
    xres: u8,
    pub client: Client,
    wins: HashMap<u32, Rc<XState>>,
    mapped: HashSet<u32>,
    net_named: HashSet<u32>,
    /// Windows the desk has (managed) or that are drawn over everything (override-redirect), in map order.
    shown: HashMap<u32, Window>,
    pub unmanaged: Vec<Window>,
    /// WL_SURFACE_ID messages whose surface bitscomp has not seen yet (the two connections race).
    ids: HashMap<u32, u32>,
    sent: HashMap<u32, Rectangle<i32, Logical>>,
    focus: u32,
    /// A focus change waiting for a server timestamp (ICCCM 4.1.7 wants one, not CurrentTime).
    focus_pending: bool,
    pub sel: crate::xsel::Sel,
}

/// Asks a window to close: an xdg toplevel by its own protocol, an X11 window through the WM at the next turn.
pub fn close(w: &Window) {
    match w {
        Window::Wl(_) => {
            if let Some(t) = w.toplevel() {
                t.send_close();
            }
        }
        Window::X(x) => x.want_close.set(true),
    }
}

impl Xwm {
    /// Queues everything that needs no answer first. Xwayland accepts on the display's sockets only once the WM
    /// owns WM_S0 (hw/xwayland/xwayland.c `wm_selection_callback`), so no client maps a window before `start`.
    pub fn new(stream: std::os::unix::net::UnixStream, client: Client) -> std::io::Result<Self> {
        let mut conn = XConn::new(stream)?;
        for (i, n) in Atoms::NAMES.iter().enumerate() {
            conn.send(xp::intern_atom(atom_name(n)), Some(Want::Atom(i)));
        }
        for e in ["Composite", "XFIXES", "X-Resource"] {
            conn.send(xp::query_extension(e), Some(Want::Ext(e)));
        }
        conn.flush();
        Ok(Xwm {
            conn,
            atoms: Atoms::default(),
            root: 0,
            wm_win: 0,
            ready: false,
            left: Atoms::NAMES.len() + 3,
            composite: 0,
            xfixes: 0,
            xfixes_event: 0,
            xres: 0,
            client,
            wins: HashMap::new(),
            mapped: HashSet::new(),
            net_named: HashSet::new(),
            shown: HashMap::new(),
            unmanaged: Vec::new(),
            ids: HashMap::new(),
            sent: HashMap::new(),
            focus: 0,
            focus_pending: false,
            sel: Default::default(),
        })
    }

    pub fn send(&mut self, req: Vec<u8>, want: Option<Want>) {
        self.conn.send(req, want);
    }

    /// A top level the WM follows (its DestroyNotify comes through the root).
    pub fn knows(&self, win: u32) -> bool {
        self.wins.contains_key(&win)
    }

    pub fn windows(&self) -> usize {
        self.shown.len()
    }

    /// Everything the X server sent since the last call.
    pub fn dispatch(&mut self, app: &mut App) {
        for m in self.conn.read() {
            match m {
                Msg::Setup(Ok(s)) => {
                    self.root = s.root;
                    eprintln!("bitscomp: xwm: connected, root 0x{:x} {}x{}", s.root, s.width, s.height);
                }
                Msg::Setup(Err(e)) => {
                    eprintln!("bitscomp: xwm: {e}");
                    self.conn.closed = true;
                }
                Msg::Reply(w, b) => self.reply(app, w, &b),
                Msg::Error(w, code, major) => {
                    if let Some(Want::Sel(s)) = w {
                        crate::xsel::failed(self, app, s);
                    } else if let Some(Want::Prop(win, atom)) = w {
                        // A property that cannot be read counts as missing (WM_PROTOCOLS then is known: none).
                        self.property(app, win, atom, xp::Prop::default());
                    } else if code != 3 {
                        // BadWindow (3) for a window that went away meanwhile is normal; the rest is logged.
                        eprintln!("bitscomp: xwm: X error {code} on request {major} ({w:?})");
                    }
                }
                Msg::Event(e) => self.event(app, &e),
            }
        }
    }

    fn reply(&mut self, app: &mut App, want: Want, b: &[u8]) {
        match want {
            Want::Atom(i) => {
                self.atoms.set(i, u32_at(b, 8));
                self.counted(app);
            }
            Want::Ext(name) => {
                let (present, op, ev) = (b[8] != 0, b[9], b[10]);
                match name {
                    "Composite" if present => self.composite = op,
                    "XFIXES" if present => (self.xfixes, self.xfixes_event) = (op, ev),
                    "X-Resource" if present => self.xres = op,
                    _ => eprintln!("bitscomp: xwm: the X server has no {name} extension"),
                }
                self.counted(app);
            }
            Want::Prop(win, atom) => self.property(app, win, atom, xp::parse_property(b)),
            Want::Pid(win) => {
                let pid = xp::xres_pid(b);
                if pid > 0 {
                    self.set_pid(app, win, pid);
                }
            }
            Want::Sel(s) => crate::xsel::reply(self, app, s, b),
            Want::Ignore => {}
        }
    }

    fn counted(&mut self, app: &mut App) {
        self.left -= 1;
        if self.left == 0 {
            self.start(app);
        }
    }

    /// All atoms and extensions known: become the window manager, then let the clients in.
    fn start(&mut self, app: &mut App) {
        if self.composite == 0 {
            eprintln!("bitscomp: xwm: no Composite: X11 windows cannot get surfaces");
            self.conn.closed = true;
            return;
        }
        let root = self.root;
        self.send(xp::ext_query_version(self.composite, 0, 4), Some(Want::Ignore));
        self.send(xp::composite_redirect_subwindows(self.composite, root), None);
        let mask = xp::SUBSTRUCTURE_REDIRECT | xp::SUBSTRUCTURE_NOTIFY | xp::PROPERTY_CHANGE | xp::FOCUS_CHANGE;
        self.send(xp::change_window_attributes(root, xp::CW_EVENT_MASK, &[mask]), None);
        let Some(wm) = self.conn.new_id() else { return };
        self.wm_win = wm;
        // An input-only window that is never mapped: the WM's name, selection owner and transfer property.
        self.send(xp::create_window(wm, root, -100, -100, 1, 1, 2, xp::CW_OVERRIDE_REDIRECT | xp::CW_EVENT_MASK, &[1, xp::PROPERTY_CHANGE]), None);
        let a = self.atoms.clone();
        self.send(xp::change_property32(wm, a._NET_SUPPORTING_WM_CHECK, xp::ATOM_WINDOW, &[wm]), None);
        self.send(xp::change_property32(root, a._NET_SUPPORTING_WM_CHECK, xp::ATOM_WINDOW, &[wm]), None);
        self.send(xp::change_property(wm, a._NET_WM_NAME, a.UTF8_STRING, 8, b"bitscomp"), None);
        let supported = [a._NET_WM_NAME, a._NET_WM_PID, a._NET_SUPPORTING_WM_CHECK, a._NET_ACTIVE_WINDOW];
        self.send(xp::change_property32(root, a._NET_SUPPORTED, xp::ATOM_ATOM, &supported), None);
        if self.xfixes != 0 {
            self.send(xp::ext_query_version(self.xfixes, 5, 0), Some(Want::Ignore));
            self.send(xp::xfixes_select_selection_input(self.xfixes, wm, a.CLIPBOARD), None);
        }
        // ICCCM 2.8: the manager selection; Xwayland starts serving the display's sockets when it is taken.
        self.send(xp::set_selection_owner(wm, a.WM_S0, 0), None);
        self.ready = true;
        self.conn.flush();
        eprintln!("bitscomp: xwm: window manager ready (composite {} xfixes {} xres {})", self.composite, self.xfixes, self.xres);
        app.needs_frame = true;
    }

    fn known(&mut self, win: u32, override_redirect: bool, geo: Rectangle<i32, Logical>) -> Rc<XState> {
        self.wins.entry(win).or_insert_with(|| Rc::new(XState::new(win, override_redirect, geo))).clone()
    }

    fn event(&mut self, app: &mut App, e: &[u8]) {
        let code = e[0] & 0x7f;
        let w = u32_at(e, 8);
        match code {
            xp::CREATE_NOTIFY => {
                if u32_at(e, 4) == self.root && w != self.wm_win {
                    let geo = Rectangle::new((i16_at(e, 12) as i32, i16_at(e, 14) as i32).into(), (u16_at(e, 16) as i32, u16_at(e, 18) as i32).into());
                    self.known(w, e[22] != 0, geo);
                }
            }
            xp::MAP_REQUEST => self.map_request(w),
            xp::MAP_NOTIFY => {
                if self.wins.contains_key(&w) {
                    self.mapped.insert(w);
                    if !self.wins[&w].override_redirect && !self.net_named.contains(&w) && self.wins[&w].class.borrow().is_empty() {
                        // Mapped before the WM redirected the root (it raced the first client): read it now.
                        self.read_props(w);
                    }
                    self.try_show(app, w);
                }
            }
            xp::UNMAP_NOTIFY => {
                if self.mapped.remove(&w) {
                    self.hide(app, w);
                    if let Some(x) = self.wins.get(&w).cloned() {
                        if !x.override_redirect {
                            let s = self.atoms.WM_STATE;
                            self.send(xp::change_property32(w, s, s, &[0, 0]), None);
                        }
                        *x.surface.borrow_mut() = None;
                    }
                }
            }
            xp::DESTROY_NOTIFY => {
                crate::xsel_serve::requestor_gone(self, w);
                self.mapped.remove(&w);
                self.hide(app, w);
                if let Some(x) = self.wins.remove(&w) {
                    x.alive.set(false);
                }
                self.net_named.remove(&w);
                self.sent.remove(&w);
                self.ids.retain(|_, x| *x != w);
            }
            xp::CONFIGURE_NOTIFY => {
                if e[0] & 0x80 == 0 && let Some(x) = self.wins.get(&w) {
                    let geo = Rectangle::new((i16_at(e, 16) as i32, i16_at(e, 18) as i32).into(), (u16_at(e, 20) as i32, u16_at(e, 22) as i32).into());
                    x.geo.set(geo);
                    if x.override_redirect {
                        app.needs_frame = true;
                    }
                }
            }
            xp::CONFIGURE_REQUEST => self.configure_request(e),
            xp::PROPERTY_NOTIFY => {
                let (win, atom, time, deleted) = (u32_at(e, 4), u32_at(e, 8), u32_at(e, 12), e[16] == 1);
                if win == self.wm_win && atom == self.atoms._BITSCOMP_TIME {
                    self.focus_at(time);
                } else if win == self.wm_win || self.sel.watches(win) {
                    crate::xsel::property_notify(self, app, win, atom, deleted);
                } else if self.wins.contains_key(&win) && self.tracked(atom) {
                    self.send(xp::get_property(false, win, atom, 1024), Some(Want::Prop(win, atom)));
                }
            }
            xp::CLIENT_MESSAGE => {
                let (win, ty) = (u32_at(e, 4), u32_at(e, 8));
                if ty == self.atoms.WL_SURFACE_ID {
                    self.ids.insert(u32_at(e, 12), win);
                    self.match_ids(app);
                }
            }
            xp::SELECTION_REQUEST | xp::SELECTION_NOTIFY | xp::SELECTION_CLEAR => crate::xsel::event(self, app, e),
            c if self.xfixes_event != 0 && c == self.xfixes_event => crate::xsel::event(self, app, e),
            _ => {}
        }
    }

    fn tracked(&self, atom: u32) -> bool {
        let a = &self.atoms;
        [xp::ATOM_WM_CLASS, xp::ATOM_WM_NAME, a._NET_WM_NAME, a.WM_PROTOCOLS, xp::ATOM_WM_TRANSIENT_FOR, xp::ATOM_WM_HINTS].contains(&atom)
    }

    fn read_props(&mut self, w: u32) {
        let a = self.atoms.clone();
        for atom in [xp::ATOM_WM_CLASS, a._NET_WM_NAME, xp::ATOM_WM_NAME, a.WM_PROTOCOLS, xp::ATOM_WM_TRANSIENT_FOR, xp::ATOM_WM_HINTS, a._NET_WM_PID] {
            self.send(xp::get_property(false, w, atom, 1024), Some(Want::Prop(w, atom)));
        }
        if self.xres != 0 {
            self.send(xp::xres_query_client_pid(self.xres, w), Some(Want::Pid(w)));
        }
    }

    fn map_request(&mut self, w: u32) {
        let x = self.known(w, false, Rectangle::default());
        if x.override_redirect {
            self.send(xp::map_window(w), None);
            return;
        }
        self.send(xp::change_window_attributes(w, xp::CW_EVENT_MASK, &[xp::PROPERTY_CHANGE | xp::FOCUS_CHANGE]), None);
        self.read_props(w);
        // ICCCM 4.1.3.1: NormalState.
        self.send(xp::change_property32(w, self.atoms.WM_STATE, self.atoms.WM_STATE, &[1, 0]), None);
        self.send(xp::map_window(w), None);
    }

    /// A window the desk places gets only its current place back; one it does not place yet gets what it asked.
    fn configure_request(&mut self, e: &[u8]) {
        let w = u32_at(e, 8);
        if let (Some(r), true) = (self.sent.get(&w).copied(), self.shown.contains_key(&w)) {
            let ev = xp::configure_notify(w, r.loc.x, r.loc.y, r.size.w as u32, r.size.h as u32);
            self.send(xp::send_event(w, xp::STRUCTURE_NOTIFY, &ev), None);
            return;
        }
        let mask = u16_at(e, 26);
        self.send(xp::configure_as_asked(w, mask, i16_at(e, 16), i16_at(e, 18), u16_at(e, 20), u16_at(e, 22), u16_at(e, 24), u32_at(e, 12), e[1]), None);
    }

    fn property(&mut self, app: &mut App, win: u32, atom: u32, p: xp::Prop) {
        let Some(x) = self.wins.get(&win).cloned() else { return };
        let a = &self.atoms;
        // Type None: the property is not there (or was deleted); WM_NAME stands in for a missing _NET_WM_NAME.
        if atom == a.WM_PROTOCOLS {
            x.protocols_read.set(true);
        }
        if p.ty == 0 {
            if atom == a.WM_PROTOCOLS {
                x.deletes.set(false);
                x.takes_focus.set(false);
            } else if atom == xp::ATOM_WM_HINTS {
                x.no_input.set(false);
            }
            if atom == a._NET_WM_NAME && self.net_named.remove(&win) {
                self.send(xp::get_property(false, win, xp::ATOM_WM_NAME, 1024), Some(Want::Prop(win, xp::ATOM_WM_NAME)));
            }
            return;
        }
        let text = || String::from_utf8_lossy(&p.value).trim_end_matches('\0').to_string();
        if atom == xp::ATOM_WM_CLASS {
            let mut parts = p.value.split(|&b| b == 0).filter(|s| !s.is_empty());
            let (inst, class) = (parts.next(), parts.next());
            *x.class.borrow_mut() = String::from_utf8_lossy(class.or(inst).unwrap_or(&[])).into_owned();
        } else if atom == a._NET_WM_NAME {
            self.net_named.insert(win);
            *x.title.borrow_mut() = text();
        } else if atom == xp::ATOM_WM_NAME {
            if !self.net_named.contains(&win) {
                // STRING is Latin-1; the common case is ASCII, for which lossy UTF-8 is the same.
                *x.title.borrow_mut() = if p.ty == xp::ATOM_STRING { p.value.iter().map(|&b| b as char).collect() } else { text() };
            }
        } else if atom == a.WM_PROTOCOLS {
            let list = p.u32s();
            x.deletes.set(list.contains(&a.WM_DELETE_WINDOW));
            x.takes_focus.set(list.contains(&a.WM_TAKE_FOCUS));
        } else if atom == xp::ATOM_WM_TRANSIENT_FOR {
            x.transient_for.set(p.u32s().first().copied().unwrap_or(0));
        } else if atom == xp::ATOM_WM_HINTS {
            // ICCCM 4.1.2.4: flags bit 0 (InputHint) says whether the second field (input) counts.
            let v = p.u32s();
            let hinted = v.first().is_some_and(|f| f & 1 != 0);
            x.no_input.set(hinted && v.get(1) == Some(&0));
        } else if atom == a._NET_WM_PID
            && let Some(&pid) = p.u32s().first()
        {
            // Only a claim (any client can write it): the X-Resource answer is the pid bitscomp uses.
            x.claimed_pid.set(pid);
        }
        if self.shown.contains_key(&win) {
            app.needs_frame = true;
        }
    }

    fn set_pid(&mut self, app: &mut App, win: u32, pid: u32) {
        let Some(x) = self.wins.get(&win) else { return };
        x.pid.set(pid);
        if self.shown.contains_key(&win) {
            app.x11_mapped(pid);
        }
    }

    fn match_ids(&mut self, app: &mut App) {
        let found: Vec<(u32, u32, WlSurface)> = self
            .ids
            .iter()
            .filter_map(|(&id, &win)| self.client.object_from_protocol_id::<WlSurface>(&app.dh, id).ok().map(|s| (id, win, s)))
            .collect();
        for (id, win, s) in found {
            self.ids.remove(&id);
            let Some(x) = self.wins.get(&win) else { continue };
            *x.surface.borrow_mut() = Some(s);
            self.try_show(app, win);
        }
    }

    fn try_show(&mut self, app: &mut App, w: u32) {
        let Some(x) = self.wins.get(&w).cloned() else { return };
        if self.shown.contains_key(&w) || !self.mapped.contains(&w) || x.wl_surface().is_none() {
            return;
        }
        let win = Window::X(x.clone());
        crate::ctl::win_id(&win);
        self.shown.insert(w, win.clone());
        app.needs_frame = true;
        if x.override_redirect {
            self.unmanaged.push(win);
            return;
        }
        eprintln!(
            "bitscomp: xwm: X11 window 0x{w:x} ({}, \"{}\") of pid {} (claims {}) is window {}",
            x.class.borrow(),
            x.title.borrow(),
            x.pid.get(),
            x.claimed_pid.get(),
            crate::ctl::win_id(&win)
        );
        app.place_new(win);
        app.layout_dirty = true;
        if x.pid.get() > 0 {
            app.x11_mapped(x.pid.get());
        }
    }

    fn hide(&mut self, app: &mut App, w: u32) {
        let Some(win) = self.shown.remove(&w) else { return };
        self.unmanaged.retain(|u| *u != win);
        self.sent.remove(&w);
        if self.focus == w {
            self.focus = 0;
        }
        if !win.x11().is_some_and(|x| x.override_redirect) {
            app.forget_window(&win);
            app.layout_dirty = true;
        }
        app.needs_frame = true;
    }

    /// Once per loop turn: surfaces that arrived, places, focus and closes the desk asked for; then the writes.
    pub fn turn(&mut self, app: &mut App) {
        if !self.ids.is_empty() {
            self.match_ids(app);
        }
        if !self.ready {
            self.conn.flush();
            return;
        }
        let a = self.atoms.clone();
        let wins: Vec<(u32, Window)> = self.shown.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (w, win) in wins {
            let Some(x) = win.x11().cloned() else { continue };
            if let Some(r) = x.want_geo.take()
                && self.sent.get(&w) != Some(&r)
            {
                self.send(xp::configure_window(w, r.loc.x, r.loc.y, r.size.w.max(1) as u32, r.size.h.max(1) as u32, false), None);
                let ev = xp::configure_notify(w, r.loc.x, r.loc.y, r.size.w as u32, r.size.h as u32);
                self.send(xp::send_event(w, xp::STRUCTURE_NOTIFY, &ev), None);
                self.sent.insert(w, r);
            }
            if x.want_kill.take() {
                self.send(xp::kill_client(w), None);
            } else if x.protocols_read.get() && x.want_close.take() {
                if x.deletes.get() {
                    let ev = xp::event(xp::CLIENT_MESSAGE, &[(4, w), (8, a.WM_PROTOCOLS), (12, a.WM_DELETE_WINDOW), (16, 0)], 32);
                    self.send(xp::send_event(w, 0, &ev), None);
                } else {
                    self.send(xp::kill_client(w), None);
                }
            }
        }
        self.sync_focus(app);
        crate::xsel_serve::turn(self);
        self.conn.flush();
    }

    /// The X input focus follows bitscomp's keyboard focus. The change waits for a server timestamp (an empty
    /// append to the WM window's _BITSCOMP_TIME answers with a PropertyNotify carrying it): ICCCM 4.1.7 wants
    /// SetInputFocus and WM_TAKE_FOCUS with a real time, and a client that passes it on must not lose to an
    /// older focus change.
    fn sync_focus(&mut self, app: &App) {
        let kb = app.seat.get_keyboard().and_then(|k| k.current_focus());
        let now = kb.and_then(|s| self.shown.iter().find(|(_, w)| w.surface().as_ref() == Some(&s)).map(|(k, _)| *k)).unwrap_or(0);
        if now == self.focus {
            return;
        }
        self.focus = now;
        if !self.focus_pending {
            self.focus_pending = true;
            let t = self.atoms._BITSCOMP_TIME;
            self.send(xp::touch_property(self.wm_win, t, xp::ATOM_STRING), None);
        }
    }

    /// The server time for the waiting focus change has come: the focus goes to the window focused now.
    fn focus_at(&mut self, time: u32) {
        if !std::mem::take(&mut self.focus_pending) {
            return;
        }
        let (now, a) = (self.focus, self.atoms.clone());
        if now == 0 || !self.wins.contains_key(&now) {
            self.send(xp::set_input_focus(0, time), None);
            self.send(xp::change_property32(self.root, a._NET_ACTIVE_WINDOW, xp::ATOM_WINDOW, &[0]), None);
            return;
        }
        let x = self.wins[&now].clone();
        self.send(xp::configure_as_asked(now, xp::CONFIG_STACK, 0, 0, 0, 0, 0, 0, 0), None);
        // ICCCM 4.1.7: input=False windows are never given the focus; they take it on WM_TAKE_FOCUS if they ask.
        if !x.no_input.get() {
            self.send(xp::set_input_focus(now, time), None);
        }
        if x.takes_focus.get() {
            let ev = xp::event(xp::CLIENT_MESSAGE, &[(4, now), (8, a.WM_PROTOCOLS), (12, a.WM_TAKE_FOCUS), (16, time)], 32);
            self.send(xp::send_event(now, 0, &ev), None);
        }
        self.send(xp::change_property32(self.root, a._NET_ACTIVE_WINDOW, xp::ATOM_WINDOW, &[now]), None);
    }

    /// Xwayland ended: every X11 window goes from the desk.
    pub fn forget_all(&mut self, app: &mut App) {
        let ws: Vec<u32> = self.shown.keys().copied().collect();
        for w in ws {
            self.hide(app, w);
        }
        for x in self.wins.values() {
            x.alive.set(false);
        }
        self.wins.clear();
        crate::xsel::forget(self, app);
    }
}
