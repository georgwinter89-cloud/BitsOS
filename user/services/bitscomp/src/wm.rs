//! Window management: turns the workspace layout into configures, the space and the keyboard focus.

use smithay::desktop::{Window, WindowSurfaceType};
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::grab::Grab;
use crate::layout::{Place, Placement, Rect};
use crate::state::App;
use crate::zones::{self, Part};

/// What lies under the pointer.
pub enum Hit {
    Surface(WlSurface, Point<f64, Logical>),
    /// bitscomp's own frame of a window: header, its buttons and tabs, borders, a floating window's edges.
    Deco(Window, Place, Part),
    /// The gap between tile `i` and `i + 1`.
    Gap(usize),
    /// bitscomp's chrome, start page, suggestions (WY-5).
    Spot(crate::desk::Spot),
    Nothing,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    None,
    Float,
    Stack,
    Unstack,
    Flip(i32),
    Focus(i32),
    Grow(f32),
    Close,
    /// End the window's program (SIGTERM, SIGKILL after 2 s: `ending`), whether it answers or not.
    Kill,
    Terminal,
    Quit,
    /// WY-5: workspaces, the address bar, Claude's sidebar.
    NewWorkspace,
    RemoveWorkspace,
    Workspace(usize),
    Address,
    /// Claude's sidebar docked or folded away (a revealed one is docked: pinned).
    Sidebar,
    /// WY-5c: the workspace list docked or folded away, as `Sidebar`.
    Workspaces,
    /// WY-5c: maximise the focused window to the whole screen, and back (a tiled one floats first).
    Maximise,
}

impl App {
    /// Where the tiles go: inside the chrome (WY-5); `tile_frames` keeps the gap at its edges.
    pub fn area(&self) -> Rect {
        self.chrome().work
    }

    /// The client's part of a frame; the rest is the server-side header and border.
    pub fn content_rect(&self, frame: Rect) -> Rect {
        let t = &self.theme;
        Rect {
            x: frame.x + t.border,
            y: frame.y + t.header_height,
            w: (frame.w - 2 * t.border).max(1),
            h: (frame.h - t.header_height - t.border).max(1),
        }
    }

    /// Where the window's buffer origin goes (its geometry may start inside the buffer, e.g. shadows).
    pub fn window_origin(&self, w: &Window, frame: Rect) -> Point<i32, Logical> {
        let c = self.content_rect(frame);
        Point::from((c.x, c.y)) - w.geometry().loc
    }

    pub fn placements(&self) -> Vec<Placement<Window>> {
        self.fit(self.ws.arrange(self.area(), self.area_full(), self.theme.gap))
    }

    /// WY-8: a fullscreen window's frame is grown by the header and border, so that its client part
    /// (`content_rect`, which every drawing and hit test uses) is exactly the screen; the frame itself lies
    /// outside the screen and is not drawn (`paint::order`).
    pub fn fit(&self, mut v: Vec<Placement<Window>>) -> Vec<Placement<Window>> {
        let t = &self.theme;
        for p in v.iter_mut().filter(|p| p.fullscreen) {
            let f = p.frame;
            p.frame = Rect { x: f.x - t.border, y: f.y - t.header_height, w: f.w + 2 * t.border, h: f.h + t.header_height + t.border };
        }
        v
    }

    /// Size and states of one placed window as pending state, sent unless the client is hung or has not had its
    /// first configure yet (then that one carries them); returns whether a configure went out.
    pub fn configure(&self, p: &Placement<Window>) -> bool {
        let c = self.content_rect(p.frame);
        let toplevel = p.win.toplevel().unwrap();
        toplevel.with_pending_state(|s| s.size = Some((c.w, c.h).into()));
        // A maximised window is `maximized` and tiled on all sides for the client: no shadows, square corners.
        let tiled = p.maximised || p.fullscreen || matches!(p.place, Place::Tiled { .. });
        App::tiled_states(toplevel, tiled, p.maximised, p.focused, p.fullscreen);
        // B4: a hung client gets no configures (they would only pile up in its socket); the pending state
        // goes out when it answers again (`answer` marks the layout dirty).
        toplevel.is_initial_configure_sent() && !self.is_hung(&p.win) && toplevel.send_pending_configure().is_some()
    }

    pub fn apply_layout(&mut self) {
        self.layout_dirty = false;
        self.needs_frame = true;
        // WY-5c: maximised floats follow the screen, the others keep their header on it (a scale change moves the
        // edges under them).
        let (full, header) = (self.area_full(), self.theme.header_height);
        self.ws.fit_floats(full, header);
        if let Some(w) = self.ws.settle_fullscreen() {
            eprintln!("bitscomp: window {} leaves fullscreen (the focus moved on)", crate::ctl::win_id(&w));
        }
        for p in self.placements() {
            self.configure(&p);
            if p.visible {
                let origin = self.window_origin(&p.win, p.frame) + p.win.geometry().loc;
                self.space.map_element(p.win.clone(), origin, false);
            } else {
                self.space.unmap_elem(&p.win);
            }
        }
        // A hung client gets no keys: they would only wait in its socket and arrive all at once later.
        let focus = self.ws.focused().filter(|w| !self.is_hung(w)).and_then(|w| w.toplevel().map(|t| t.wl_surface().clone()));
        let kb = self.seat.get_keyboard().unwrap();
        if kb.current_focus() != focus {
            kb.set_focus(self, focus, SERIAL_COUNTER.next_serial());
        }
    }

    pub fn forget_window(&mut self, w: &Window) {
        self.ws.remove(w);
        self.forget_parked(w);
        self.anchors.retain(|_, a| a != w);
        self.pending_open.iter_mut().for_each(|p| {
            if p.1.as_ref() == Some(w) {
                p.1 = None;
            }
        });
        self.space.unmap_elem(w);
        if let Some(id) = w.toplevel().map(|t| t.wl_surface().id()) {
            self.deco.retain(|(k, _), _| *k != id);
            self.corner_img.retain(|(k, _), _| *k != id);
            self.veil.remove(&id);
        }
        self.deco_img.retain(|k, _| w.toplevel().is_none_or(|t| *k != t.wl_surface().id()));
        if self.grab.as_ref().is_some_and(|g| g.window() == Some(w)) {
            self.grab = None;
        }
        if self.menu.as_ref().is_some_and(|m| m.target.as_ref() == Some(w)) {
            self.menu = None;
        }
        if self.hover.as_ref().is_some_and(|(h, _)| h == w) {
            self.hover = None;
        }
        if self.last_click.as_ref().is_some_and(|(h, _, _)| h == w) {
            self.last_click = None;
        }
        self.layout_dirty = true;
        self.session_dirty();
    }

    /// The suggestions first, then floating windows (WY-5c: above every bar), then the chrome and the start page,
    /// the gaps, the tiles.
    pub fn hit(&self, pos: Point<f64, Logical>) -> Hit {
        if let Some(i) = self.suggest_at(pos.x, pos.y) {
            return Hit::Spot(crate::desk::Spot::Suggest(i));
        }
        let placements: Vec<_> = self.placements().into_iter().rev().filter(|p| p.visible).collect();
        if let Some(p) = placements.iter().find(|p| p.fullscreen) {
            // WY-8: the whole screen is the client's: no bars, no frame, no resize edges (a hung one keeps the
            // veil's buttons).
            return match self.hit_window(p, pos) {
                Some(h @ (Hit::Surface(..) | Hit::Deco(_, _, Part::Kill | Part::Wait))) => h,
                _ => Hit::Nothing,
            };
        }
        let (floats, tiles): (Vec<_>, Vec<_>) =
            placements.into_iter().partition(|p| matches!(p.place, Place::Floating { .. }));
        for p in floats {
            if !p.maximised && let Some(e) = zones::resize_edges(p.frame, &self.theme, pos.x, pos.y) {
                return Hit::Deco(p.win, p.place, Part::Resize(e));
            }
            if let Some(h) = self.hit_window(&p, pos) {
                return h;
            }
        }
        if let Some(s) = self.spot_at(pos.x, pos.y) {
            return Hit::Spot(s);
        }
        if self.chrome().covers(pos.x, pos.y) {
            return Hit::Nothing;
        }
        let frames = self.ws.tile_frames(self.area(), self.theme.gap);
        if let Some(i) = zones::gap_at(&frames, &self.theme, pos.x, pos.y) {
            return Hit::Gap(i);
        }
        tiles.iter().find_map(|p| self.hit_window(p, pos)).unwrap_or(Hit::Nothing)
    }

    /// Whether a floating window lies under (x, y) (its frame or its resize margin).
    pub fn float_at(&self, x: f64, y: f64) -> bool {
        let m = self.theme.resize_margin;
        self.ws.floating.iter().any(|f| Rect { x: f.rect.x - m, y: f.rect.y - m, w: f.rect.w + 2 * m, h: f.rect.h + 2 * m }.contains(x, y))
    }

    fn hit_window(&self, p: &Placement<Window>, pos: Point<f64, Logical>) -> Option<Hit> {
        if self.is_hung(&p.win) {
            // The veil is bitscomp's: its button ends the program, the rest of it is like the frame's body.
            let body = self.content_rect(p.frame);
            if crate::veil::shown(self, &p.win) {
                if crate::veil::kill_button(body, &self.theme).contains(pos.x, pos.y) {
                    return Some(Hit::Deco(p.win.clone(), p.place, Part::Kill));
                }
                if crate::veil::wait_button(body, &self.theme).contains(pos.x, pos.y) {
                    return Some(Hit::Deco(p.win.clone(), p.place, Part::Wait));
                }
            }
            return p.frame.contains(pos.x, pos.y).then(|| {
                Hit::Deco(p.win.clone(), p.place, zones::header_part(p.frame, p.place, &self.theme, pos.x, pos.y))
            });
        }
        if let Some(s) = self.page_hit(&p.win, p.frame, pos.x, pos.y) {
            return Some(Hit::Spot(s));
        }
        let origin = self.window_origin(&p.win, p.frame);
        if let Some((s, loc)) = p.win.surface_under(pos - origin.to_f64(), WindowSurfaceType::ALL) {
            return Some(Hit::Surface(s, (loc + origin).to_f64()));
        }
        p.frame
            .contains(pos.x, pos.y)
            .then(|| Hit::Deco(p.win.clone(), p.place, zones::header_part(p.frame, p.place, &self.theme, pos.x, pos.y)))
    }

    /// A client's own "move" request (e.g. a drag on Brave's tab strip) floats the window and moves it.
    pub fn begin_move(&mut self, w: &Window) {
        if matches!(self.ws.find(w), Some(Place::Tiled { .. })) {
            self.ws.toggle_float(w, self.area());
        }
        self.ws.focus_window(w);
        if let Some(Place::Floating { index }) = self.ws.find(w) {
            // A maximised window gets its old size back under the pointer, as a header drag does.
            self.unmax_at(index, self.pointer.x, self.pointer.y);
            let r = self.ws.floating[index].rect;
            let grab = self.pointer - Point::from((r.x as f64, r.y as f64));
            self.grab = Some(Grab::MoveFloat { window: w.clone(), grab });
        }
        self.layout_dirty = true;
    }

    /// Runs an action on a given window (the circle menu, a header button) instead of the focused one.
    pub fn act_on(&mut self, target: Option<&Window>, action: Action) {
        if let Some(w) = target {
            if self.ws.find(w).is_none() {
                return;
            }
            self.ws.focus_window(w);
        }
        self.act(action);
    }

    pub fn act(&mut self, action: Action) {
        let focused = self.ws.focused().cloned();
        match action {
            Action::None => return,
            Action::Float => {
                if let Some(w) = focused {
                    self.ws.toggle_float(&w, self.area());
                }
            }
            Action::Stack => {
                self.ws.stack_focused();
            }
            Action::Unstack => {
                self.ws.unstack_focused();
            }
            Action::Flip(dir) => {
                if let crate::layout::Focus::Tile(t) = self.ws.focus {
                    self.ws.flip(t, dir);
                }
            }
            Action::Focus(dir) => self.ws.focus_step(dir),
            Action::Grow(d) => {
                self.ws.grow_focused(d);
            }
            Action::Close => {
                if let Some(t) = focused.as_ref().and_then(|w| w.toplevel()) {
                    t.send_close();
                }
            }
            Action::Kill => {
                if let Some(w) = focused {
                    let _ = self.end_window(&w, "Programm beenden");
                }
            }
            Action::Terminal => {
                self.open_program(crate::terminal::TERMINAL.iter().map(|s| s.to_string()).collect(), crate::address::Where::Tile, None);
            }
            Action::Quit => self.quit = true,
            Action::NewWorkspace => {
                self.ws_new();
            }
            Action::RemoveWorkspace => {
                self.ws_remove(self.desk.active);
            }
            Action::Workspace(i) => {
                self.ws_switch(i);
            }
            Action::Address => {
                let tile = match (self.theme.layout.address_top, self.ws.focus) {
                    (false, crate::layout::Focus::Tile(t)) => Some(t),
                    _ => None,
                };
                self.address_begin(tile);
            }
            Action::Sidebar => {
                let docked = self.theme.layout.sidebar;
                self.set_bar(crate::desk::SIDEBAR_KEY, !docked);
            }
            Action::Workspaces => {
                let docked = self.theme.layout.workspaces;
                self.set_bar(crate::desk::WORKSPACES_KEY, !docked);
            }
            Action::Maximise => {
                // WY-8: on a fullscreen window the desktop's own maximise is the way out of it (Esc and F11 are
                // the client's); it does not maximise as well.
                if let Some(w) = focused.as_ref().filter(|w| self.ws.is_fullscreen(w)) {
                    self.ws.set_fullscreen(w, false);
                    eprintln!("bitscomp: window {} leaves fullscreen (Maximise)", crate::ctl::win_id(w));
                } else if let Some(w) = focused {
                    if matches!(self.ws.find(&w), Some(Place::Tiled { .. })) {
                        self.ws.toggle_float(&w, self.area());
                    }
                    self.ws.toggle_max(&w, self.area_full());
                    eprintln!("bitscomp: window {} {}", crate::ctl::win_id(&w), if self.ws.is_max(&w) { "maximised" } else { "restored" });
                }
            }
        }
        self.layout_dirty = true;
    }

    pub fn spawn(&mut self, argv: &[&str]) -> Option<u32> {
        let (cmd, args) = argv.split_first()?;
        // The child's PATH (spawn_env wins over ours): the trampoline's execvp searches that one.
        let path = crate::spawn::child_path(&self.spawn_env, std::env::var("PATH").ok());
        if !crate::spawn::found(cmd, &path, crate::spawn::executable) {
            eprintln!("bitscomp: cannot start {cmd}: not found (PATH={path})");
            return None;
        }
        let parent = std::process::id() as libc::pid_t;
        // WY-7: through the trampoline (posix_spawn, no fork of the compositor: `spawn`), else forked as before.
        let mut c = match crate::spawn::trampoline() {
            Some(t) => {
                let mut c = std::process::Command::new(t);
                c.arg("exec").arg(parent.to_string()).arg(cmd);
                c
            }
            None => {
                let mut c = std::process::Command::new(cmd);
                // A crashed or killed bitscomp takes its started programs with it (SIGTERM), as a clean end does.
                // SAFETY: prctl and getppid are async-signal-safe and touch no memory of the parent; ENOSYS/EINVAL
                // on a kernel without PR_SET_PDEATHSIG is ignored. A parent already gone before the prctl: the
                // child ends itself.
                unsafe {
                    std::os::unix::process::CommandExt::pre_exec(&mut c, move || {
                        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                        if libc::getppid() != parent {
                            libc::raise(libc::SIGTERM);
                        }
                        crate::spawn::unblock_all();
                        Ok(())
                    });
                }
                c
            }
        };
        // WY-5b: foot gets the theme's colours; only the exec sees it (the session keeps `argv`).
        let foot = crate::foot::is_foot(cmd);
        let foot_file = foot && self.foot_config_arg().map(|a| c.arg(a)).is_some();
        c.args(args).env("WAYLAND_DISPLAY", &self.socket_name).env_remove("DISPLAY");
        for (k, v) in &self.spawn_env {
            c.env(k, v);
        }
        // Never bitscomp's own stdin (a root start's console terminal: not the user's to read or write).
        c.stdin(std::process::Stdio::null());
        // Own process group (pgid = its pid): the SIGTERM/SIGKILL at bitscomp's end goes to the group and so also
        // reaches helpers it started in the same group.
        std::os::unix::process::CommandExt::process_group(&mut c, 0);
        match c.spawn() {
            Ok(child) => {
                let pid = child.id();
                self.children.push((child, std::time::Instant::now()));
                // WY-7: the start is answered on the screen at the next frame (the "starting" mark).
                self.launches.started(pid, crate::pacer::now());
                self.needs_frame = true;
                self.spawned.insert(pid, argv.iter().map(|a| a.to_string()).collect());
                if foot {
                    self.foot.started(pid, foot_file);
                }
                self.page_spawned(pid, argv);
                Some(pid)
            }
            Err(e) => {
                eprintln!("bitscomp: cannot start {cmd}: {e}");
                None
            }
        }
    }

    pub fn reap_children(&mut self) {
        self.children.retain_mut(|(c, _)| !matches!(c.try_wait(), Ok(Some(_))));
        if self.spawned.len() > self.children.len() {
            let alive: Vec<u32> = self.children.iter().map(|(c, _)| c.id()).collect();
            if self.launches.retain_alive(|p| alive.contains(&p)) {
                self.needs_frame = true;
            }
            self.spawned.retain(|p, _| alive.contains(p));
            self.foot.retain(|p| alive.contains(&p));
            self.pages_reap();
        }
    }
}
