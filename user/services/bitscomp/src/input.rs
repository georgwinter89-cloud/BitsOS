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
    /// WY-20: the same from a relative device, with its motion as it came (`lock::relative_event`).
    Relative {
        x: f64,
        y: f64,
        rel: crate::lock::Rel,
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

/// WY-12: the direction of Alt+Tab (forward) or Shift+Alt+Tab (back).
fn switch_dir(sym: Keysym, shift: bool) -> Option<i32> {
    match sym {
        Keysym::ISO_Left_Tab => Some(-1),
        Keysym::Tab => Some(if shift { -1 } else { 1 }),
        _ => None,
    }
}

/// Where an input event came from (R8c). Only `Physical` input may answer the installer's dialog (MC-9b): the
/// back end decides it per device (evdev: MCP's injected devices and any `BUS_VIRTUAL` device are `Injected`),
/// the control socket's `key`/`click`/... are always `Injected`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Physical,
    Injected,
}

impl Origin {
    pub fn name(self) -> &'static str {
        match self {
            Origin::Physical => "physical",
            Origin::Injected => "injected",
        }
    }
}

/// Ctrl+Alt+1..9 by key position (evdev KEY_1..KEY_9 = 2..10), whatever the layout puts there.
fn workspace_key(code: Keycode) -> Option<Action> {
    let c = code.raw().checked_sub(8)?;
    (2..=10).contains(&c).then(|| Action::Workspace(c as usize - 2))
}

impl App {
    /// An event of the control socket: `Injected`, unless the headless test path `phys` set `ctl_origin`.
    pub fn handle_input(&mut self, ev: InputEvent) {
        let o = self.ctl_origin;
        self.handle_input_as(ev, o);
    }

    /// An event from a back end, with the origin of its device.
    pub fn handle_input_as(&mut self, ev: InputEvent, origin: Origin) {
        self.input_origin = origin;
        let time = self.now_ms();
        // PK-L1b: a button or a wheel step while Win is down makes it a combination (no library on its release).
        if matches!(ev, InputEvent::Button { pressed: true, .. } | InputEvent::Wheel { .. } | InputEvent::HWheel { .. }) {
            self.lib.alone.other();
        }
        // MC-9b: while the overlay is shown, the wheel scrolls its text and reaches no window.
        if self.install.shown() {
            match ev {
                InputEvent::Wheel { steps } => return self.install_wheel(steps),
                InputEvent::HWheel { .. } => return,
                _ => {}
            }
        }
        if self.lib.open && matches!(ev, InputEvent::HWheel { .. }) {
            return;
        }
        match ev {
            InputEvent::Key { code, pressed } => self.key(code, pressed, time),
            InputEvent::Motion { x, y } => self.pointer_to(x, y, None, time),
            InputEvent::Relative { x, y, rel } => self.pointer_to(x, y, Some(rel), time),
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

    fn pointer_to(&mut self, x: f64, y: f64, rel: Option<crate::lock::Rel>, time: u32) {
        let t_in = std::time::Instant::now();
        let s = self.theme.scale;
        // WY-20: the client gets the relative motion; a lock keeps the pointer where it is, a confinement near.
        let Some(to) = self.lock_motion((x / s, y / s).into(), rel) else { return };
        self.pointer = to;
        self.motion(time);
        // WY-11: the shape bitscomp chose for this spot, then the hardware cursor moves now, not at a frame.
        self.cursor_sync();
        self.cursor_moved(t_in);
        // A software pointer (or the "starting" mark beside a hardware one) needs a frame.
        if self.draw_pointer && (!self.cursor.on_hw || self.launches.busy()) {
            self.needs_frame = true;
        }
    }

    fn key(&mut self, code: u32, pressed: bool, time: u32) {
        let keycode = Keycode::new(code + 8);
        let state = if pressed { KeyState::Pressed } else { KeyState::Released };
        let kb = self.seat.get_keyboard().unwrap();
        // PK-L1b (D7): Win pressed and released alone toggles the library. The Super key itself still reaches the
        // client as before (its press and release both), so its modifier state there never sticks.
        let alone = self.lib.alone.key(code, pressed, time);
        let action =
            kb.input::<Action, _>(self, keycode, state, SERIAL_COUNTER.next_serial(), time, |app, mods, sym| {
                // MC-9b: the installer's overlay takes every key first (nothing reaches a window or a shortcut
                // while it is shown); Ctrl+Alt+G opens the grant dialog (real keyboard only).
                if pressed && app.install_takes_keys() {
                    app.intercepted.insert(keycode);
                    app.install_key(sym.modified_sym());
                    return FilterResult::Intercept(Action::None);
                }
                // DS-5: a key while the boot scene is up opens the desktop at once and types nothing (as the
                // kernel's boot screen keeps its key from the shell).
                if pressed && app.images.boot.shown() {
                    app.intercepted.insert(keycode);
                    app.boot_key();
                    return FilterResult::Intercept(Action::None);
                }
                if pressed && mods.ctrl && mods.alt && sym.modified_sym() == Keysym::g {
                    app.intercepted.insert(keycode);
                    app.install_grant_open();
                    return FilterResult::Intercept(Action::None);
                }
                // MC-10: Ctrl+Alt+V opens (or ends) full access for the paired Gaming-PC (real keyboard only).
                if pressed && mods.ctrl && mods.alt && sym.modified_sym() == Keysym::v {
                    app.intercepted.insert(keycode);
                    app.access_open();
                    return FilterResult::Intercept(Action::None);
                }
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
                // WY-5c: Super+Up maximises (and restores), as on classic desktops; WY-12: Super+Down minimises.
                // Not while the library is open: the window under the dim would change unseen (the arrows move
                // the library's focus instead).
                if pressed && !app.lib.open && mods.logo && !mods.ctrl && !mods.alt && sym.modified_sym() == Keysym::Up {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::Maximise);
                }
                if pressed && !app.lib.open && mods.logo && !mods.ctrl && !mods.alt && sym.modified_sym() == Keysym::Down {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::Minimize);
                }
                // PK-L1b: Win+Esc (GM-6's sofa mode), Win+Space (Feld), Win+Tab (workspaces) and Win+Z (rewind) are
                // kept for their parts (desktop.md §0 point 6): bitscomp takes them, unbound for now.
                if pressed
                    && mods.logo
                    && !mods.ctrl
                    && !mods.alt
                    && matches!(sym.modified_sym(), Keysym::Escape | Keysym::space | Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::z | Keysym::Z)
                {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
                }
                // WY-12: Alt+Tab and Shift+Alt+Tab switch windows, minimised ones too (Shift turns Tab into
                // ISO_Left_Tab in most layouts).
                if pressed && mods.alt && !mods.ctrl && !mods.logo && let Some(dir) = switch_dir(sym.modified_sym(), mods.shift) {
                    app.intercepted.insert(keycode);
                    // The switch shows another window: the library goes first, so it is not switched to unseen.
                    app.library_close("Alt+Tab");
                    return FilterResult::Intercept(Action::Switch(dir));
                }
                // PK-L1b: the open library takes every key but the modifiers (they reach the client as before).
                if pressed && app.lib.open && app.library_key(sym.modified_sym(), mods) {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
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
        // The installer's question and the boot scene keep the keys first (MC-9b, DS-5): no library over them.
        if alone && !self.install.shown() && !self.images.boot.shown() {
            self.library_toggle("Win alone");
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
