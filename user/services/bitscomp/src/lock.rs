//! WY-20: mouse look. zwp_relative_pointer_manager_v1 gives a client the device's motion as it comes (accelerated and
//! raw, also at the screen's edge) and zwp_pointer_constraints_v1 holds the pointer still (a lock) or inside a region
//! (a confinement); SDL's relative mode needs both, and Xwayland makes XI2 raw motion from the first and grabs from
//! the second. A constraint is active only for the focused window with the pointer over it, inside its region, and
//! while nothing of bitscomp's takes the pointer (`wanted`). It ends at focus loss and when the window goes, and comes
//! back with both if the client made it persistent. A lock hides the pointer; everything about the keyboard stays as
//! it is, so Esc reaches the client and the desktop's shortcuts work.

use std::time::Duration;

use smithay::backend::renderer::utils::with_renderer_surface_state;
use smithay::input::pointer::{PointerHandle, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{DisplayHandle, Resource};
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::pointer_constraints::{
    PointerConstraint, PointerConstraintsHandler, PointerConstraintsState, with_pointer_constraint,
};
use smithay::wayland::relative_pointer::RelativePointerManagerState;

use crate::backend::evdev_map::{accel, moved};
use crate::input::InputEvent;
use crate::state::App;

type Pos = Point<f64, Logical>;

/// One relative motion. `delta` in the surface's coordinates after the acceleration, as the pointer would move;
/// `raw` the device's counts, neither accelerated nor scaled, so a game's sensitivity ignores the desktop's scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rel {
    pub delta: (f64, f64),
    pub raw: (f64, f64),
}

struct Held {
    surface: WlSurface,
    locked: bool,
    /// Where the client wants the pointer when the lock ends, in the surface's coordinates.
    hint: Option<Pos>,
}

pub struct Lock {
    _relative: RelativePointerManagerState,
    _constraints: PointerConstraintsState,
    held: Option<Held>,
    /// What the pointer is over (`App::motion`) and the keyboard's focus (`focus_changed`: the keyboard is locked
    /// during that call, so nothing here asks it).
    under: Option<(WlSurface, Pos)>,
    keyboard: Option<WlSurface>,
    /// Something `wanted` looks at changed: `lock_sync` looks.
    dirty: bool,
    /// What `blocked` said at the last look: the menu, drag, dialog and boot scene end in many places.
    blocked: bool,
    quiet: Quiet,
}

/// A client can take and drop a constraint in a loop; the log is not to follow (B6): this many lines a second, and the
/// next line says how many were held back.
const LOG_PER_SECOND: u32 = 8;

#[derive(Default)]
struct Quiet {
    since: Duration,
    lines: u32,
    held_back: u64,
}

impl Quiet {
    /// Whether a line goes out at `now`, and how many went unsaid before it.
    fn line(&mut self, now: Duration) -> Option<u64> {
        if now.saturating_sub(self.since) >= Duration::from_secs(1) {
            self.since = now;
            self.lines = 0;
        }
        if self.lines >= LOG_PER_SECOND {
            self.held_back += 1;
            return None;
        }
        self.lines += 1;
        Some(std::mem::take(&mut self.held_back))
    }
}

impl Lock {
    pub fn new(dh: &DisplayHandle) -> Lock {
        Lock {
            _relative: RelativePointerManagerState::new::<App>(dh),
            _constraints: PointerConstraintsState::new::<App>(dh),
            held: None,
            under: None,
            keyboard: None,
            dirty: false,
            blocked: false,
            quiet: Quiet::default(),
        }
    }

    fn note(&mut self, what: std::fmt::Arguments<'_>) {
        match self.quiet.line(crate::pacer::now()) {
            Some(0) => eprintln!("bitscomp: pointer: {what}"),
            Some(n) => eprintln!("bitscomp: pointer: {what} ({n} lines held back)"),
            None => {}
        }
    }

    /// The pointer is not to be seen.
    pub fn hides(&self) -> bool {
        self.held.as_ref().is_some_and(|h| h.locked)
    }

    pub fn pointer_over(&mut self, under: Option<(WlSurface, Pos)>) {
        self.under = under;
        self.dirty = true;
    }

    pub fn keyboard_on(&mut self, focused: Option<&WlSurface>) {
        self.keyboard = focused.cloned();
        self.dirty = true;
    }

    pub fn changed(&mut self) {
        self.dirty = true;
    }
}

/// The pointer's next place inside `size` at `origin` and, if the client gave one, inside its region: an axis that
/// would leave stays where it was, so the pointer slides along an edge.
pub fn confine(from: Pos, to: Pos, origin: Pos, size: (f64, f64), inside: impl Fn(Pos) -> bool) -> Pos {
    let clamped = |at: f64, o: f64, len: f64| at.clamp(o, o + (len - 1.0).max(0.0));
    let to: Pos = (clamped(to.x, origin.x, size.0), clamped(to.y, origin.y, size.1)).into();
    [to, (to.x, from.y).into(), (from.x, to.y).into()].into_iter().find(|p| inside(*p)).unwrap_or(from)
}

/// Where a client's hint puts the pointer when a lock ends. Only a wish, and wl_fixed reaches far past any screen:
/// outside the surface None, so the pointer stays; onto the output if part of the surface lies beyond it.
pub fn hint_place(hint: Pos, origin: Pos, size: (f64, f64), output: (f64, f64)) -> Option<Pos> {
    let inside = (0.0..size.0).contains(&hint.x) && (0.0..size.1).contains(&hint.y);
    let onto = |at: f64, len: f64| at.clamp(0.0, (len - 1.0).max(0.0));
    let at = origin + hint;
    inside.then(|| (onto(at.x, output.0), onto(at.y, output.1)).into())
}

fn surface_size(surface: &WlSurface) -> Option<(f64, f64)> {
    let size: Size<i32, Logical> = with_renderer_surface_state(surface, |st| st.surface_size()).flatten()?;
    Some((f64::from(size.w), f64::from(size.h)))
}

/// A report of a relative device as the input event, from the pointer's place now.
pub fn relative_event(app: &App, dx: f64, dy: f64) -> InputEvent {
    let mode = app.output.current_mode().map(|m| m.size).unwrap_or_default();
    let s = app.theme.scale;
    let (x, y) = moved((app.pointer.x * s, app.pointer.y * s), (dx, dy), (mode.w, mode.h));
    let (a, b) = accel(dx, dy);
    InputEvent::Relative { x, y, rel: Rel { delta: (a / s, b / s), raw: (dx, dy) } }
}

impl App {
    /// Something of bitscomp's takes the pointer.
    fn lock_blocked(&self) -> bool {
        self.install.shown() || self.menu.is_some() || self.grab.is_some() || self.images.boot.shown()
    }

    /// Whether a constraint is on the pointer's surface and may be active now, and which surface that is.
    fn lock_wanted(&self, ptr: &PointerHandle<App>) -> Option<WlSurface> {
        if self.lock_blocked() {
            return None;
        }
        let (surface, origin) = self.lock.under.as_ref().filter(|(s, _)| s.is_alive())?;
        let at = (self.pointer - *origin).to_i32_round();
        // First what a desktop without games answers quickly: this runs after every motion.
        if !with_pointer_constraint(surface, ptr, |c| c.is_some_and(|c| c.region().is_none_or(|r| r.contains(at)))) {
            return None;
        }
        let keyboard = self.lock.keyboard.as_ref().filter(|k| k.is_alive())?;
        let same = ptr.current_focus().as_ref() == Some(surface)
            && self.window_of_surface(surface)? == self.window_of_surface(keyboard)?;
        same.then(|| surface.clone())
    }

    /// Brings the active constraint up to date (every loop turn and before pointer input; quick when nothing changed).
    pub fn lock_sync(&mut self) {
        let blocked = self.lock_blocked();
        if blocked != self.lock.blocked {
            self.lock.blocked = blocked;
            self.lock.dirty = true;
            // While it lasted no window had the pointer. The first report would move a free pointer and only then
            // find the lock, so the window gets it back now (a pointer that did not leave it has nothing to redo).
            if !blocked && self.lock.under.is_none() {
                self.motion(self.now_ms());
            }
        }
        if !self.lock.dirty && self.lock.held.is_none() {
            return;
        }
        self.lock.dirty = false;
        let ptr = self.seat.get_pointer().unwrap();
        // The client destroyed it, or Smithay ended it when the pointer left (wl_pointer leave deactivates).
        if let Some(h) = &self.lock.held
            && !(h.surface.is_alive()
                && with_pointer_constraint(&h.surface, &ptr, |c| c.is_some_and(|c| c.is_active())))
        {
            self.lock_ended(&ptr, "the constraint or the surface went");
        }
        let want = self.lock_wanted(&ptr);
        if self.lock.held.as_ref().map(|h| &h.surface) == want.as_ref() {
            return;
        }
        if let Some(h) = &self.lock.held {
            with_pointer_constraint(&h.surface, &ptr, |c| {
                if let Some(c) = c {
                    c.deactivate();
                }
            });
            self.lock_ended(&ptr, "focus or pointer moved on");
        }
        let Some(surface) = want else { return };
        let locked = with_pointer_constraint(&surface, &ptr, |c| {
            c.map(|c| {
                c.activate();
                matches!(&*c, PointerConstraint::Locked(_))
            })
        });
        let Some(locked) = locked else { return };
        self.lock.note(format_args!("{} for surface {}", if locked { "locked" } else { "confined" }, surface.id()));
        self.lock.held = Some(Held { surface, locked, hint: None });
        if locked {
            self.lock_shape();
        }
    }

    fn lock_ended(&mut self, ptr: &PointerHandle<App>, why: &str) {
        let Some(h) = self.lock.held.take() else { return };
        self.lock.note(format_args!("released from surface {}: {why}", h.surface.id()));
        if !h.locked {
            return;
        }
        self.lock_shape();
        // The client's wish, while it still has the pointer and nothing of the desktop's took it (a menu opens there).
        if let (Some(hint), Some((s, origin))) = (h.hint, &self.lock.under)
            && *s == h.surface
            && s.is_alive()
            && ptr.current_focus().as_ref() == Some(s)
            && !self.lock.blocked
            && let Some(size) = surface_size(s)
            && let Some(mode) = self.output.current_mode()
            && let Some(place) = hint_place(hint, *origin, size, self.lock_output(mode.size.w, mode.size.h))
        {
            self.pointer = place;
            self.motion(self.now_ms());
            self.cursor_moved(std::time::Instant::now());
        }
    }

    fn lock_output(&self, w: i32, h: i32) -> (f64, f64) {
        (f64::from(w) / self.theme.scale, f64::from(h) / self.theme.scale)
    }

    fn lock_shape(&mut self) {
        if self.draw_pointer {
            self.needs_frame = true;
        }
        self.cursor_sync();
    }

    /// Pointer input going to `to`. Sends the relative motion first, in the wl_pointer frame the motion ends; the
    /// place the pointer goes to, or None when a lock keeps it (then no motion, and no frame on the screen either).
    /// Without a device's own numbers (an absolute device, the control socket's `pointer`) the distance counts.
    pub(crate) fn lock_motion(&mut self, to: Pos, rel: Option<Rel>) -> Option<Pos> {
        self.lock_sync();
        let s = self.theme.scale;
        let rel = rel.unwrap_or_else(|| {
            let d = to - self.pointer;
            Rel { delta: (d.x, d.y), raw: (d.x * s, d.y * s) }
        });
        let ptr = self.seat.get_pointer().unwrap();
        let under = self.lock.under.clone().filter(|(s, _)| s.is_alive());
        let moving = rel.raw != (0.0, 0.0) || rel.delta != (0.0, 0.0);
        if under.is_some() && moving {
            let utime = crate::pacer::now().as_micros() as u64;
            ptr.relative_motion(
                self,
                under,
                &RelativeMotionEvent { delta: rel.delta.into(), delta_unaccel: rel.raw.into(), utime },
            );
        }
        match &self.lock.held {
            Some(h) if h.locked => {
                ptr.frame(self);
                None
            }
            Some(h) => Some(self.lock_confined(&h.surface.clone(), to, &ptr)),
            None => Some(to),
        }
    }

    fn lock_confined(&self, surface: &WlSurface, to: Pos, ptr: &PointerHandle<App>) -> Pos {
        let Some((_, origin)) = self.lock.under.as_ref().filter(|(s, _)| s == surface) else { return self.pointer };
        let Some(size) = surface_size(surface) else { return self.pointer };
        let region = with_pointer_constraint(surface, ptr, |c| c.and_then(|c| c.region().cloned()));
        confine(self.pointer, to, *origin, size, |p| {
            region.as_ref().is_none_or(|r| r.contains((p - *origin).to_i32_round()))
        })
    }

    /// `ctl lock`: what the pointer is held by, and where it is.
    pub fn lock_report(&self) -> String {
        let (x, y) = (self.pointer.x, self.pointer.y);
        match &self.lock.held {
            Some(h) => format!(
                "lock state={} surface={} hidden={} pointer={x:.2},{y:.2}",
                if h.locked { "locked" } else { "confined" },
                h.surface.id(),
                self.lock.hides()
            ),
            None => format!("lock state=none pointer={x:.2},{y:.2}"),
        }
    }
}

impl PointerConstraintsHandler for App {
    fn new_constraint(&mut self, _surface: &WlSurface, _pointer: &PointerHandle<Self>) {
        self.lock.changed();
    }

    /// The last wish counts; one outside the region (which is also where a persistent lock could come back) is none.
    fn cursor_position_hint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>, location: Pos) {
        let at = location.to_i32_round();
        let inside =
            with_pointer_constraint(surface, pointer, |c| c.is_some_and(|c| c.region().is_none_or(|r| r.contains(at))));
        if let Some(h) = self.lock.held.as_mut().filter(|h| h.surface == *surface) {
            h.hint = inside.then_some(location);
        }
    }
}

smithay::delegate_relative_pointer!(App);
smithay::delegate_pointer_constraints!(App);

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Pos {
        (x, y).into()
    }

    #[test]
    fn a_confinement_keeps_the_pointer_on_the_surface() {
        let any = |_: Pos| true;
        let (o, size) = (p(100.0, 50.0), (200.0, 100.0));
        assert_eq!(confine(p(150.0, 60.0), p(160.0, 70.0), o, size, any), p(160.0, 70.0));
        assert_eq!(
            confine(p(150.0, 60.0), p(900.0, 70.0), o, size, any),
            p(299.0, 70.0),
            "slides along the right edge"
        );
        assert_eq!(confine(p(150.0, 60.0), p(10.0, -5.0), o, size, any), p(100.0, 50.0));
    }

    #[test]
    fn a_loop_of_constraints_does_not_flood_the_log() {
        let mut q = Quiet::default();
        let t = Duration::from_secs(100);
        let out = (0..1000).filter(|_| q.line(t).is_some()).count() as u32;
        assert_eq!(out, LOG_PER_SECOND);
        assert_eq!(q.line(t + Duration::from_millis(999)), None);
        assert_eq!(
            q.line(t + Duration::from_secs(1)),
            Some(u64::from(1001 - LOG_PER_SECOND)),
            "the next second says so"
        );
        assert_eq!(q.line(t + Duration::from_secs(1)), Some(0));
    }

    #[test]
    fn a_hint_is_only_a_wish() {
        let (o, size, screen) = (p(100.0, 50.0), (200.0, 100.0), (1024.0, 640.0));
        assert_eq!(hint_place(p(20.0, 30.0), o, size, screen), Some(p(120.0, 80.0)));
        assert_eq!(hint_place(p(0.0, 0.0), o, size, screen), Some(p(100.0, 50.0)));
        assert_eq!(hint_place(p(200.0, 30.0), o, size, screen), None, "just past the surface");
        assert_eq!(hint_place(p(-1.0, 30.0), o, size, screen), None, "before it");
        assert_eq!(hint_place(p(8_000_000.0, 8_000_000.0), o, size, screen), None, "what wl_fixed can say");
        // A surface half off the output: the pointer stays on the output.
        let off = hint_place(p(190.0, 90.0), p(900.0, 600.0), size, screen);
        assert_eq!(off, Some(p(1023.0, 639.0)));
    }

    #[test]
    fn a_region_keeps_an_axis_that_stays_inside() {
        let (o, size) = (p(0.0, 0.0), (500.0, 500.0));
        let square = |q: Pos| q.x >= 10.0 && q.x < 110.0 && q.y >= 10.0 && q.y < 110.0;
        assert_eq!(confine(p(50.0, 50.0), p(60.0, 60.0), o, size, square), p(60.0, 60.0));
        assert_eq!(confine(p(50.0, 50.0), p(300.0, 60.0), o, size, square), p(50.0, 60.0), "x would leave, y moves");
        assert_eq!(confine(p(50.0, 50.0), p(60.0, 300.0), o, size, square), p(60.0, 50.0), "y would leave, x moves");
        assert_eq!(confine(p(50.0, 50.0), p(300.0, 300.0), o, size, square), p(50.0, 50.0), "both would: it stays");
    }
}
