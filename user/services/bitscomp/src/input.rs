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
    /// An event of the control socket: `Injected` (a unit test may set `ctl_origin`).
    pub fn handle_input(&mut self, ev: InputEvent) {
        let o = self.ctl_origin;
        self.handle_input_as(ev, o);
    }

    /// An event from a back end, with the origin of its device.
    pub fn handle_input_as(&mut self, ev: InputEvent, origin: Origin) {
        // CL-5: while Georg types a provider's key, injected input reaches nothing: it could take the keyboard
        // to a field that shows what he types. Releases pass, so nothing it pressed before stays held.
        let release = matches!(ev, InputEvent::Key { pressed: false, .. } | InputEvent::Button { pressed: false, .. });
        if !release && self.ki_drops(origin) {
            return;
        }
        self.input_origin = origin;
        if origin == Origin::Physical {
            self.leistung_input();
            // EINGABE: Georg's own key, click or move stops the desktop writing for Durchblick (a release of
            // what he pressed before, e.g. the Enter that ran the step, does not).
            if !release {
                self.desk_write_hand();
            }
        }
        let own_press = origin == Origin::Physical && matches!(ev, InputEvent::Button { pressed: true, .. });
        // DB1: Georg's own press goes into the history (and ctxd's habits) with the element it lands on.
        if own_press {
            self.ui_note_press();
        }
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
        if (self.lib.open || self.feld.open || self.rewind.open) && matches!(ev, InputEvent::HWheel { .. }) {
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
        if own_press {
            self.ki_own_click();
        }
    }

    fn pointer_to(&mut self, x: f64, y: f64, rel: Option<crate::lock::Rel>, time: u32) {
        let t_in = std::time::Instant::now();
        let s = self.theme.scale;
        // WY-20: the client gets the relative motion; a lock keeps the pointer where it is, a confinement near.
        let Some(to) = self.lock_motion((x / s, y / s).into(), rel) else { return };
        self.pointer = to;
        self.pointer_origin = self.input_origin;
        self.motion(time);
        // WY-11: the shape bitscomp chose for this spot, then the hardware cursor moves now, not at a frame.
        self.cursor_sync();
        self.cursor_moved(t_in);
        // A software pointer (or the "starting" mark beside a hardware one) needs a frame.
        if !self.cursor.on_hw || self.launches.busy() {
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
                // CL-5: a provider's key was being typed and its field lost the keyboard without Georg ending
                // it: his keys go nowhere (not to whatever took the keyboard) until his next click.
                if pressed && app.ki_key_swallowed() {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
                }
                if pressed && mods.ctrl && mods.alt && sym.modified_sym() == Keysym::g {
                    app.intercepted.insert(keycode);
                    app.install_grant_open();
                    return FilterResult::Intercept(Action::None);
                }
                // A (12.11.): the question about closing a workspace takes every key while it is open.
                if pressed && app.desk.ws_ask.is_some() {
                    app.intercepted.insert(keycode);
                    app.ws_ask_key(sym.modified_sym());
                    return FilterResult::Intercept(Action::None);
                }
                // DB3: the Durchblick key opens its panel; while open the panel has the keyboard.
                if pressed && app.db_key(sym.modified_sym(), mods) {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
                }
                // MC-10: Ctrl+Alt+V opens (or ends) full access for the paired Gaming-PC (real keyboard only).
                if pressed && mods.ctrl && mods.alt && sym.modified_sym() == Keysym::v {
                    app.intercepted.insert(keycode);
                    app.access_open();
                    return FilterResult::Intercept(Action::None);
                }
                // SYS-A: Strg+Umschalt+Esc opens the task manager (the program "System", tab Prozesse), as on other
                // desktops; before Esc's own uses below, which never come with Ctrl and Shift.
                if pressed && mods.ctrl && mods.shift && !mods.alt && !mods.logo && sym.modified_sym() == Keysym::Escape {
                    app.intercepted.insert(keycode);
                    app.sys_shortcut();
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
                // C (12.11.): Super+Shift+Up brings the newest minimised window of this workspace back (Super+Up is
                // maximise's).
                if pressed && !app.lib.open && !app.feld.open && !app.rewind.open && !app.sofa.listens() && mods.logo && !mods.ctrl && !mods.alt && sym.modified_sym() == Keysym::Up {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(if mods.shift { Action::Unminimize } else { Action::Maximise });
                }
                if pressed && !app.lib.open && !app.feld.open && !app.rewind.open && !app.sofa.listens() && mods.logo && !mods.ctrl && !mods.alt && sym.modified_sym() == Keysym::Down {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::Minimize);
                }
                // PK-L1b: Win+Esc switches the sofa mode (GM-6a), Win+Space opens or closes the Feld (FELD); REWIND:
                // Win+Tab and Shift+Win+Tab switch to the next and the previous workspace, Win+Z rewinds
                // (desktop.md §0 point 6).
                if pressed
                    && mods.logo
                    && !mods.ctrl
                    && !mods.alt
                    && matches!(sym.modified_sym(), Keysym::Escape | Keysym::space | Keysym::Tab | Keysym::ISO_Left_Tab | Keysym::z | Keysym::Z)
                {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(match sym.modified_sym() {
                        Keysym::Escape => Action::Sofa,
                        Keysym::space => Action::Feld,
                        s @ (Keysym::Tab | Keysym::ISO_Left_Tab) => switch_dir(s, mods.shift).map_or(Action::None, Action::CycleWorkspace),
                        _ => Action::Rewind,
                    });
                }
                // WY-12: Alt+Tab and Shift+Alt+Tab switch windows, minimised ones too (Shift turns Tab into
                // ISO_Left_Tab in most layouts).
                if pressed && mods.alt && !mods.ctrl && !mods.logo && let Some(dir) = switch_dir(sym.modified_sym(), mods.shift) {
                    app.intercepted.insert(keycode);
                    // The switch shows another window: the library goes first, so it is not switched to unseen.
                    app.library_close("Alt+Tab");
                    app.feld_close("Alt+Tab");
                    app.rewind_close("Alt+Tab");
                    return FilterResult::Intercept(Action::Switch(dir));
                }
                // GM-6a: a sofa screen or the quick menu takes every key but the modifiers (behind a program in
                // front the keys are the program's; Win+Esc above still switches).
                if pressed && app.sofa.listens() && app.sofa_key(sym.modified_sym(), mods) {
                    app.intercepted.insert(keycode);
                    // GM-6d: a key held on the search screen repeats (bitscomp's own: no client sees it).
                    app.sofa_hold(keycode, sym.modified_sym(), *mods);
                    return FilterResult::Intercept(Action::None);
                }
                // PK-L1b: the open library takes every key but the modifiers (they reach the client as before).
                if pressed && app.lib.open && app.library_key(sym.modified_sym(), mods) {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
                }
                // REWIND: the open rewind view takes every key but the modifiers.
                if pressed && app.rewind.open && app.rewind_key(sym.modified_sym(), mods) {
                    app.intercepted.insert(keycode);
                    return FilterResult::Intercept(Action::None);
                }
                // FELD: the open Feld takes every key but the modifiers, with the address bar's key repeat.
                if pressed && app.feld.open && app.feld_key(sym.modified_sym(), mods) {
                    app.intercepted.insert(keycode);
                    app.address_hold(keycode, sym.modified_sym(), *mods);
                    return FilterResult::Intercept(Action::None);
                }
                // The sidebar's open model list takes every key but the modifiers.
                if pressed && app.assist_menu_open() && app.assist_menu_key(sym.modified_sym(), mods, app.input_origin) {
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
                // CL-3b: the assistant's input line takes every other key while it has the focus.
                if pressed && app.assist_takes_keys() {
                    app.intercepted.insert(keycode);
                    app.assist_key(sym.modified_sym(), mods, app.input_origin);
                    app.address_hold(keycode, sym.modified_sym(), *mods);
                    return FilterResult::Intercept(Action::None);
                }
                // WY-10: a page's confirmation dialog takes the keys (Esc, Enter, Tab, arrows) while it is focused.
                if pressed && let Some(pid) = app.page_dialog_pid() {
                    app.intercepted.insert(keycode);
                    app.page_dialog_key(pid, sym.modified_sym());
                    return FilterResult::Intercept(Action::None);
                }
                // PE-1: Esc closes the shown program-error card when nothing above took it (one Esc, then the keys are
                // the program's again), so a card goes without the mouse.
                if pressed && sym.modified_sym() == Keysym::Escape && !mods.ctrl && !mods.alt && !mods.logo && !mods.shift && app.prog_note_shown().is_some() {
                    app.intercepted.insert(keycode);
                    app.prog_note_close();
                    return FilterResult::Intercept(Action::None);
                }
                // B (12.11.): the start page over the workspace's windows: their keys stay with bitscomp (the
                // windows are not shown); Esc brings them back as a second "Startseite" does.
                if pressed && app.desk.start_forced {
                    app.intercepted.insert(keycode);
                    if sym.modified_sym() == Keysym::Escape {
                        app.home_press();
                    }
                    return FilterResult::Intercept(Action::None);
                }
                if !pressed {
                    app.address_release(keycode);
                    app.sofa_release(keycode);
                }
                if !pressed && app.intercepted.remove(&keycode) {
                    return FilterResult::Intercept(Action::None);
                }
                if pressed {
                    app.stats.keys_forwarded += 1;
                }
                FilterResult::Forward
            });
        // CL-5: a shortcut of Georg's own ends a key's field as he would (his next keys are his).
        let own = self.input_origin == Origin::Physical && (alone || action.as_ref().is_some_and(|a| !matches!(a, Action::None)));
        if own {
            self.ki_key_own_end();
        }
        if let Some(a) = action {
            self.act(a);
        }
        // The installer's question and the boot scene keep the keys first (MC-9b, DS-5): no library over them.
        // GM-6a: not in the sofa mode (its home is its own library, GM-6b).
        if alone && !self.install.shown() && !self.images.boot.shown() && !self.sofa.active {
            self.library_toggle("Win alone");
        }
    }

}
