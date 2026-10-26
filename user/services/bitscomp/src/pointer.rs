//! The mouse ways of plan/spec/desktop.md §1 ("Everything by mouse too"): click focuses; header buttons and
//! tabs (a press on a tab shows it, its x closes that window); header and tab drags (stack, beside, own tile; a floating window moves); floating edges resize; gaps
//! change the split; right click opens the circle menu. The middle button is never bitscomp's: it goes to the
//! client under the pointer (reserved for "run text", desktop.md §5); on a header there is none, so nothing.

use smithay::backend::input::{Axis, AxisSource, ButtonState};
use smithay::desktop::Window;
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorIcon, MotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::SERIAL_COUNTER;

use crate::grab::Grab;
use crate::grab::resize_icon;
use crate::layout::Place;
use crate::menu::{self, MenuHit};
use crate::state::App;
use crate::wm::{Action, Hit};
use crate::zones::{self, Part};

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

impl App {
    pub(crate) fn motion(&mut self, time: u32) {
        let pos = self.pointer;
        let mut focus = None;
        self.reveal_motion(crate::pacer::now());
        if let Some(m) = &self.menu {
            let hover = match menu::hit(m.centre, &m.items, &self.theme, pos.x, pos.y) {
                MenuHit::Item(d) => Some(d),
                _ => None,
            };
            if hover != m.hover {
                self.menu.as_mut().unwrap().hover = hover;
                self.needs_frame = true;
            }
            self.cursor_deco = Some(CursorIcon::Default);
        } else if let Some(g) = self.grab.take() {
            self.grab = Some(self.grab_motion(g, pos));
        } else {
            let hit = self.hit(pos);
            let hover = match &hit {
                Hit::Deco(
                    w,
                    _,
                    p @ (Part::Float
                    | Part::Max
                    | Part::Close
                    | Part::Tab(_)
                    | Part::TabClose(_)
                    | Part::More
                    | Part::Kill
                    | Part::Wait
                    | Part::Address),
                ) => Some((w.clone(), *p)),
                _ => None,
            };
            if hover != self.hover {
                self.hover = hover;
                self.needs_frame = true;
            }
            let spot = match &hit {
                Hit::Spot(s) => Some(*s),
                _ => None,
            };
            if spot != self.desk.hover {
                self.desk.hover = spot;
                self.chrome_dirty();
            }
            self.cursor_deco = match hit {
                Hit::Surface(s, loc) => {
                    focus = Some((s, loc));
                    None
                }
                Hit::Deco(_, _, Part::Resize(e)) => Some(resize_icon(e)),
                Hit::Gap(_) => Some(CursorIcon::ColResize),
                _ => Some(CursorIcon::Default),
            };
        }
        let ptr = self.seat.get_pointer().unwrap();
        ptr.motion(self, focus, &MotionEvent { location: pos, serial: SERIAL_COUNTER.next_serial(), time });
        ptr.frame(self);
    }

    pub(crate) fn button(&mut self, code: u32, pressed: bool, time: u32) {
        let kept = if code == BTN_MIDDLE {
            false
        } else if pressed {
            self.press(code)
        } else {
            self.swallowed.remove(&code) && {
                self.release(code);
                true
            }
        };
        if kept {
            if pressed {
                self.swallowed.insert(code);
            }
            return;
        }
        // A client's own move request started a grab on a press the client got; its release ends it.
        if !pressed && code == BTN_LEFT && self.grab.take().is_some() {
            self.needs_frame = true;
        }
        let state = if pressed { ButtonState::Pressed } else { ButtonState::Released };
        let ptr = self.seat.get_pointer().unwrap();
        ptr.button(self, &ButtonEvent { button: code, state, serial: SERIAL_COUNTER.next_serial(), time });
        ptr.frame(self);
    }

    /// Returns whether bitscomp keeps the press (and so its release) for itself.
    fn press(&mut self, code: u32) -> bool {
        let pos = self.pointer;
        if self.menu.is_some() {
            self.menu_press();
            return true;
        }
        if self.grab.is_some() {
            return true;
        }
        let hit = self.hit(pos);
        if !matches!(hit, Hit::Spot(_) | Hit::Deco(_, _, Part::Address)) {
            self.address_end();
        }
        if !matches!(hit, Hit::Spot(_)) {
            self.page_edit_end();
        }
        match hit {
            Hit::Spot(s) => self.spot_press(s, code == BTN_RIGHT),
            Hit::Surface(s, _) => {
                if let Some(w) = self.window_of_surface(&s)
                    && self.ws.focused() != Some(&w)
                {
                    self.ws.focus_window(&w);
                    self.layout_dirty = true;
                }
                false
            }
            Hit::Deco(w, place, part) => {
                // A tab press shows its window, as a browser does; the tab's x leaves the shown page alone.
                let tab = match part {
                    Part::Tab(i) | Part::TabClose(i) => self.tab_window(&w, place, i),
                    _ => None,
                };
                let target = tab.clone().unwrap_or_else(|| w.clone());
                self.ws.focus_window(if matches!(part, Part::Tab(_)) { &target } else { &w });
                self.layout_dirty = true;
                // WY-5c: a double click on a floating window's header maximises it, or gives it its place back.
                if code == BTN_LEFT && self.float_header(&w, place, part, pos.y) {
                    let now = crate::pacer::now();
                    let prev = self.last_click.take().filter(|(lw, _, _)| *lw == w).map(|(_, t, p)| (t, p));
                    if zones::is_double(prev, now, (pos.x, pos.y), self.theme.drag_threshold) {
                        self.act_on(Some(&w), Action::Maximise);
                        return true;
                    }
                    self.last_click = Some((w.clone(), now, (pos.x, pos.y)));
                } else {
                    self.last_click = None;
                }
                if code == BTN_RIGHT {
                    self.open_menu(Some(target));
                } else if code == BTN_LEFT {
                    self.grab = match part {
                        Part::Body | Part::Tab(_) => Some(Grab::Pending { window: target, start: pos }),
                        Part::Float | Part::Max | Part::Close | Part::More | Part::TabClose(_) | Part::Kill | Part::Wait | Part::Address => {
                            Some(Grab::Button { window: target, part })
                        }
                        Part::Resize(edges) => {
                            // The press raised the window, so its index is looked up again.
                            let Some(Place::Floating { index }) = self.ws.find(&w) else { return true };
                            Some(Grab::Resize { window: w, edges, start: pos, rect: self.ws.floating[index].rect })
                        }
                    };
                }
                true
            }
            Hit::Gap(gap) if code == BTN_LEFT => {
                self.grab = Some(Grab::Split { gap });
                true
            }
            Hit::Gap(_) | Hit::Nothing => {
                if code == BTN_RIGHT {
                    self.open_menu(None);
                }
                true
            }
        }
    }

    fn release(&mut self, code: u32) {
        if self.desk.press.is_some() {
            if code == BTN_LEFT {
                self.spot_release();
            } else {
                self.desk.press = None;
            }
            return;
        }
        if self.menu.is_some() {
            if code == BTN_RIGHT {
                self.menu_release();
            }
            return;
        }
        if code != BTN_LEFT {
            return;
        }
        match self.grab.take() {
            Some(Grab::MoveTile { window, drop }) => {
                self.ws.move_window(&window, drop);
                self.layout_dirty = true;
            }
            Some(Grab::Button { window, part }) => {
                if let Hit::Deco(w, place, p) = self.hit(self.pointer)
                    && p == part
                {
                    let on = match p {
                        Part::TabClose(i) => self.tab_window(&w, place, i),
                        _ => Some(w),
                    };
                    if on.as_ref() == Some(&window) {
                        self.button_action(&window, place, part);
                    }
                }
            }
            _ => {}
        }
        self.needs_frame = true;
        // The pointer may now be over something else than where the grab began.
        self.motion(self.now_ms());
    }

    fn button_action(&mut self, window: &Window, place: Place, part: Part) {
        match (part, place) {
            (Part::Close, _) => self.act_on(Some(window), Action::Close),
            (Part::Float, _) => self.act_on(Some(window), Action::Float),
            (Part::Max, _) => self.act_on(Some(window), Action::Maximise),
            (Part::Kill, _) => {
                let _ = self.end_window(window, "Beenden");
            }
            (Part::Wait, _) => self.veil_wait_for(window),
            (Part::Address, Place::Tiled { tile, .. }) => self.address_begin(Some(tile)),
            (Part::More, Place::Tiled { tile, .. }) => {
                self.ws.flip(tile, 1);
                self.layout_dirty = true;
            }
            // Closing a hidden page must not show it first.
            (Part::TabClose(_), _) => {
                if let Some(t) = window.toplevel() {
                    t.send_close();
                }
            }
            _ => {}
        }
    }

    /// Whether a press on `part` of `w` at height `y` is on a floating window's header (not on its buttons).
    fn float_header(&self, w: &Window, place: Place, part: Part, y: f64) -> bool {
        // `place` is from before the press raised the window: look it up again.
        let (Place::Floating { .. }, Some(Place::Floating { index })) = (place, self.ws.find(w)) else { return false };
        let f = &self.ws.floating[index];
        matches!(part, Part::Body | Part::Tab(_)) && y < (f.rect.y + self.theme.header_height) as f64
    }

    /// The window of tab `page` in the header of `w`.
    pub(crate) fn tab_window(&self, w: &Window, place: Place, page: usize) -> Option<Window> {
        match place {
            Place::Tiled { tile, .. } => self.ws.tiles.get(tile)?.stack.get(page).cloned(),
            Place::Floating { .. } => Some(w.clone()),
        }
    }

    /// The wheel on a tile's header turns its pages; elsewhere it goes to the client.
    pub(crate) fn wheel(&mut self, steps: f64, time: u32) {
        if let Hit::Spot(crate::desk::Spot::Page(pid, _)) = self.hit(self.pointer) {
            return self.page_scroll(pid, steps);
        }
        if let Hit::Deco(_, Place::Tiled { tile, .. }, _) = self.hit(self.pointer) {
            if self.ws.flip(tile, if steps > 0.0 { 1 } else { -1 }) {
                self.layout_dirty = true;
            }
            return;
        }
        let frame = AxisFrame::new(time)
            .source(AxisSource::Wheel)
            .value(Axis::Vertical, steps * 15.0)
            .v120(Axis::Vertical, (steps * 120.0) as i32);
        let ptr = self.seat.get_pointer().unwrap();
        ptr.axis(self, frame);
        ptr.frame(self);
    }

    fn window_of_surface(&self, s: &WlSurface) -> Option<Window> {
        let mut root = s.clone();
        while let Some(p) = smithay::wayland::compositor::get_parent(&root) {
            root = p;
        }
        if let Some(w) = self.find_window(&root) {
            return Some(w);
        }
        // A popup: find the window it belongs to.
        let popup = self.popups.find_popup(&root)?;
        let top = smithay::desktop::find_popup_root_surface(&popup).ok()?;
        self.find_window(&top)
    }
}
