//! Input in back-end-neutral form: evdev key and button codes (what `backend::evdev` reads on V2), pointer
//! positions in output pixels. Compositor shortcuts are Ctrl+Alt+key so they work nested inside KDE.

use smithay::backend::input::{Axis, AxisSource, KeyState};
use smithay::input::pointer::AxisFrame;
use smithay::input::keyboard::{FilterResult, Keycode, Keysym};
use smithay::utils::SERIAL_COUNTER;

use crate::state::App;
use crate::wm::Action;

pub enum InputEvent {
    Key {
        code: u32,
        pressed: bool,
    },
    /// Position in physical output pixels.
    Motion {
        x: f64,
        y: f64,
    },
    Button {
        code: u32,
        pressed: bool,
    },
    /// Wheel steps, positive = down (Wayland's direction).
    Wheel {
        steps: f64,
    },
    /// Horizontal wheel steps, positive = right.
    HWheel {
        steps: f64,
    },
}

fn binding(sym: Keysym) -> Option<Action> {
    Some(match sym {
        Keysym::Return => Action::Terminal,
        Keysym::f => Action::Float,
        Keysym::s => Action::Stack,
        Keysym::u => Action::Unstack,
        Keysym::Next => Action::Flip(1),
        Keysym::Prior => Action::Flip(-1),
        Keysym::Right => Action::Focus(1),
        Keysym::Left => Action::Focus(-1),
        Keysym::plus | Keysym::KP_Add => Action::Grow(0.25),
        Keysym::minus | Keysym::KP_Subtract => Action::Grow(-0.25),
        // WY-5c: W is the workspace list now (Georg's brief); closing moved to X, like the header's [x].
        Keysym::x => Action::Close,
        Keysym::w => Action::Workspaces,
        Keysym::c => Action::Sidebar,
        Keysym::m => Action::Maximise,
        Keysym::q => Action::Quit,
        Keysym::l => Action::Address,
        Keysym::n => Action::NewWorkspace,
        _ => return None,
    })
}

/// Ctrl+Alt+1..9 by key position (evdev KEY_1..KEY_9 = 2..10), whatever the layout puts there.
fn workspace_key(code: Keycode) -> Option<Action> {
    let c = code.raw().checked_sub(8)?;
    (2..=10).contains(&c).then(|| Action::Workspace(c as usize - 2))
}

impl App {
    pub fn handle_input(&mut self, ev: InputEvent) {
        let time = self.now_ms();
        match ev {
            InputEvent::Key { code, pressed } => self.key(code, pressed, time),
            InputEvent::Motion { x, y } => {
                let t_in = std::time::Instant::now();
                let s = self.theme.scale;
                self.pointer = (x / s, y / s).into();
                self.motion(time);
                // WY-11: the shape bitscomp chose for this spot, then the hardware cursor moves now, not at a frame.
                self.cursor_sync();
                self.cursor_moved(t_in);
                // A software pointer (or the "starting" mark beside a hardware one) needs a frame.
                if self.draw_pointer && (!self.cursor.on_hw || self.launches.busy()) {
                    self.needs_frame = true;
                }
            }
            InputEvent::Button { code, pressed } => self.button(code, pressed, time),
            InputEvent::Wheel { steps } => self.wheel(steps, time),
            InputEvent::HWheel { steps } => {
                let frame = AxisFrame::new(time)
                    .source(AxisSource::Wheel)
                    .value(Axis::Horizontal, steps * 15.0)
                    .v120(Axis::Horizontal, (steps * 120.0) as i32);
                let ptr = self.seat.get_pointer().unwrap();
                ptr.axis(self, frame);
                ptr.frame(self);
            }
        }
    }

    fn key(&mut self, code: u32, pressed: bool, time: u32) {
        let keycode = Keycode::new(code + 8);
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        let kb = self.seat.get_keyboard().unwrap();
        let action =
            kb.input::<Action, _>(self, keycode, state, SERIAL_COUNTER.next_serial(), time, |app, mods, sym| {
                // Esc closes the circle menu or drops a header drag, and stays with bitscomp.
                if pressed && sym.modified_sym() == Keysym::Escape && app.cancel_pointer_modes() {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
                }
                if pressed
                    && mods.ctrl
                    && mods.alt
                    && let Some(a) = workspace_key(keycode).or_else(|| binding(sym.modified_sym()))
                {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(a);
                }
                // WY-5c: Super+Up maximises (and restores), as on classic desktops.
                if pressed && mods.logo && !mods.ctrl && !mods.alt && sym.modified_sym() == Keysym::Up {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::Maximise);
                }
                // A settings page's field being edited takes every other key (WY-5b).
                if pressed && let Some(pid) = app.page_edit_pid() {
                    app.intercepted.insert(keycode);
                    app.page_key(pid, sym.modified_sym(), mods);
                    app.address_hold(keycode, sym.modified_sym(), *mods);
                    return FilterResult::Intercept(Action::None);
                }
                // The address bar being edited takes every other key (WY-5).
                if pressed && app.desk.edit.is_some() {
                    app.intercepted.insert(keycode);
                    app.address_key(sym.modified_sym(), mods);
                    app.address_hold(keycode, sym.modified_sym(), *mods);
                    return FilterResult::Intercept(Action::None);
                }
                // WY-10: a page's confirmation dialog takes the keys (Esc, Enter, Tab, arrows) while it is focused.
                if pressed && let Some(pid) = app.page_dialog_pid() {
                    app.intercepted.insert(keycode);
                    app.page_dialog_key(pid, sym.modified_sym());
                    return FilterResult::Intercept(Action::None);
                }
                if !pressed {
                    app.address_release(keycode);
                }
                if !pressed && app.intercepted.remove(&keycode) {
                    return FilterResult::Intercept(Action::None);
                }
                if pressed {
                    app.stats.keys_forwarded += 1;
                }
                FilterResult::Forward
            });
        if let Some(a) = action {
            self.act(a);
        }
    }

    /// Releases every held key, e.g. when the nested window loses the host's keyboard focus.
    #[cfg_attr(not(feature = "nested"), allow(dead_code))]
    pub fn release_all_keys(&mut self) {
        let kb = self.seat.get_keyboard().unwrap();
        let time = self.now_ms();
        for k in kb.pressed_keys() {
            self.key(k.raw() - 8, false, time);
        }
    }
}
