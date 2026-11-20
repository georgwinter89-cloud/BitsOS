//! DB3, EINS-B: the popup by keyboard, mouse and gamepad. The key (`durchblick.taste`) opens and closes it; while
//! it is open it has the keyboard and the input line has the focus: Enter asks (on the empty line "Erklär mir,
//! was ich hier sehe"), Esc closes. At a step: Enter = Ja, Esc = Nein (board 2); while the assistant acts, Esc
//! stops it at once, also when no popup is open (the bar says so). Tab and Shift+Tab move between the input line,
//! the buttons and the pill, the arrows scroll the text. On the gamepad Select with the chosen button opens it, B
//! closes (or stops), A presses, Y opens the sofa mode's keyboard for a question.

use smithay::input::keyboard::{Keysym, ModifiersState};

use crate::db_app::Stage;
use crate::db_layout::Button;
use crate::db_settings::Taste;
use crate::pad::{Btn, Dir, Press};
use crate::state::App;

impl App {
    /// A key: true when the popup took it (the key that opens it, anything while it is open, Esc while the
    /// assistant acts).
    pub fn db_key(&mut self, sym: Keysym, mods: &ModifiersState) -> bool {
        // DB5b: a step's own keys go to the window it acts in.
        if self.act.stepping {
            return false;
        }
        if Taste::of(&self.settings).hits(sym, mods) {
            self.db_toggle(Taste::of(&self.settings).name());
            return true;
        }
        // The bar's promise: Esc stops the assistant wherever its chat is shown.
        if sym == Keysym::Escape && self.db_acting() && !(self.db.open && self.db.stage == Stage::Step) {
            self.db_act_stop(crate::db_act_app::STOP_NOTE);
            self.needs_frame = true;
            return true;
        }
        if !self.db.open || self.db_sofa_typing() || crate::library_ui::is_modifier(sym) {
            return false;
        }
        let field = self.db.has_field();
        let origin = self.input_origin;
        match sym {
            Keysym::Escape if self.db.level_open => self.db.level_open = false,
            Keysym::Escape if self.db.stage == Stage::Step => self.db_press(Button::StepNo),
            Keysym::Escape => self.db_close("Esc"),
            Keysym::Tab | Keysym::ISO_Left_Tab => self.db_focus_step(if mods.shift || sym == Keysym::ISO_Left_Tab { -1 } else { 1 }),
            // A focus on a button that is gone (the level's choices after the pick) is the input line's again.
            Keysym::Return | Keysym::KP_Enter => match self.db.focus.filter(|b| self.db_buttons().contains(b)) {
                Some(b) => self.db_press(b),
                None if field => {
                    let text = self.db.field.text.clone();
                    self.db_submit(&text, origin);
                }
                None => {}
            },
            Keysym::Up | Keysym::KP_Up | Keysym::Prior => self.db.scroll = self.db.scroll.saturating_sub(if sym == Keysym::Prior { 10 } else { 1 }),
            Keysym::Down | Keysym::KP_Down | Keysym::Next => self.db.scroll += if sym == Keysym::Next { 10 } else { 1 },
            _ if field => {
                let e = &mut self.db.field;
                match sym {
                    Keysym::BackSpace => e.backspace(),
                    Keysym::Delete | Keysym::KP_Delete => e.delete(),
                    Keysym::Left | Keysym::KP_Left => e.left(),
                    Keysym::Right | Keysym::KP_Right => e.right(),
                    Keysym::Home | Keysym::KP_Home => e.home(),
                    Keysym::End | Keysym::KP_End => e.end(),
                    _ => match sym.key_char() {
                        Some(c) if !c.is_control() && !mods.ctrl && !mods.alt && !mods.logo && e.text.chars().count() < 2000 => e.insert(c),
                        _ => {}
                    },
                }
                // Typing goes to the input line: the focus leaves the buttons.
                self.db.focus = None;
            }
            _ => {}
        }
        self.needs_frame = true;
        true
    }

    /// The focus to the next (`by` 1) or the previous stop: the input line (where there is one), the buttons.
    fn db_focus_step(&mut self, by: i32) {
        let mut stops: Vec<Option<Button>> = Vec::new();
        if self.db.has_field() {
            stops.push(None);
        }
        stops.extend(self.db_buttons().into_iter().map(Some));
        if stops.is_empty() {
            return;
        }
        let now = stops.iter().position(|s| *s == self.db.focus).unwrap_or(0) as i32;
        let n = stops.len() as i32;
        self.db.focus = stops[((now + by) % n + n) as usize % stops.len()];
    }

    /// Whether a press at the pointer is the panel's.
    pub fn db_takes_press(&self) -> bool {
        let (x, y) = (self.pointer.x, self.pointer.y);
        self.db_geo().is_some_and(|g| g.panel.contains(x, y))
    }

    /// A press on the panel: a button, the pill, the input line.
    pub fn db_click(&mut self) {
        let Some(g) = self.db_geo() else { return };
        let (x, y) = (self.pointer.x, self.pointer.y);
        if let Some((b, _)) = g.buttons.iter().find(|(_, r)| r.contains(x, y)) {
            self.db.focus = Some(*b);
            return self.db_press(*b);
        }
        if g.field.contains(x, y) {
            self.db.focus = None;
            self.needs_frame = true;
        }
    }

    /// A gamepad press: true when the popup took it.
    pub fn db_pad(&mut self, p: Press) -> bool {
        if p == Press::Btn(Btn::Help) {
            if crate::db_settings::pad_button(&self.settings).is_some() {
                self.db_toggle(crate::db_app::PAD_OPEN);
            }
            return true;
        }
        if p == Press::Btn(Btn::B) && self.db_acting() && !(self.db.open && self.db.stage == Stage::Step) {
            self.db_act_stop(crate::db_act_app::STOP_NOTE);
            return true;
        }
        if !self.db.open || self.db_sofa_typing() {
            return false;
        }
        match p {
            Press::Btn(Btn::B) if self.db.stage == Stage::Step => self.db_press(Button::StepNo),
            Press::Btn(Btn::B) => self.db_close("gamepad B"),
            Press::Btn(Btn::A) => match self.db.focus {
                Some(b) => self.db_press(b),
                None if self.db.has_field() && !self.db.field.text.trim().is_empty() => {
                    let (text, origin) = (self.db.field.text.clone(), self.input_origin);
                    self.db_submit(&text, origin);
                }
                None => self.db_press(Button::Keyboard),
            },
            Press::Btn(Btn::Y) if self.db.has_field() => self.db_press(Button::Keyboard),
            Press::Move(Dir::Left | Dir::Up) => self.db_focus_step(-1),
            Press::Move(Dir::Right | Dir::Down) => self.db_focus_step(1),
            Press::Btn(Btn::LB) => self.db.scroll = self.db.scroll.saturating_sub(5),
            Press::Btn(Btn::RB) => self.db.scroll += 5,
            _ => {}
        }
        self.needs_frame = true;
        true
    }

    /// The gamepads' Select chord button from the setting.
    pub fn db_pad_sync(&mut self) {
        let help = crate::db_settings::pad_button(&self.settings);
        for (_, d) in &mut self.pads.list {
            d.pad.help = help;
        }
    }
}
