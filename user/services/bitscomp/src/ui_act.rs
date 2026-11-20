//! DB1: `ui-act <element-id> <action> [text]`: does what the element offers as a real input would: the pointer
//! to the element's centre and a click (press, choose, toggle, focus), keys on the German layout after a click
//! into the field (type), the wheel over it (scroll). Everything goes through `handle_input` with the control
//! socket's origin, so confirm dialogs still come and dialogs that count only real input (installer, access)
//! refuse it. A row scrolled out of sight is first scrolled to by the wheel over its window. An element covered
//! by something else at its point is refused (the click would land there). An agent over MCP never presses the
//! button that answers a question (`answers`): that stays Georg's. A foreign window's element is acted on by its
//! provider (C3 `act`).

use uilist::{Action, Element, Role, State};

use crate::input::InputEvent;
use crate::pointer::BTN_LEFT;
use crate::state::App;
use crate::ui_keys::{ALTGR, Mod, SHIFT, key_of, untypable};
use crate::ui_reg::Full;

/// Rounds of wheel steps tried at most to bring a hidden element into view, and the most steps sent at once.
const REVEAL_ROUNDS: usize = 12;
const MAX_STEPS: f64 = 500.0;
const KEY_END: u32 = 107;
const KEY_BACKSPACE: u32 = 14;

/// Who acts: the control socket (scripts on this machine) or an agent over MCP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Ctl,
    Mcp,
}

pub fn words(a: Action) -> &'static str {
    match a {
        Action::Press => "press",
        Action::Type => "type",
        Action::Choose => "choose",
        Action::Toggle => "toggle",
        Action::Scroll => "scroll",
        Action::Focus => "focus",
        Action::Submit => "submit",
    }
}

/// What an agent over MCP (and Durchblick, which acts as one) never does: answer a question, and (DB5b) what
/// Durchblick never does (secret and payment input, Confirm settings).
pub(crate) fn agent_limits(by: By, w: &uilist::Window, e: &Element, a: Action, text: Option<&str>) -> Result<(), String> {
    if by != By::Mcp {
        return Ok(());
    }
    if e.answers {
        return Err(format!("„{}“ beantwortet eine Rückfrage; das muss Georg selbst bestätigen. Es wurde nichts getan.", e.name));
    }
    crate::db_act::limits(w, e, a, text)
}

impl App {
    /// The window and the element of `id` now.
    fn ui_lookup(&self, id: &str) -> Result<(uilist::Window, Element), String> {
        let id = uilist::clean_id(id);
        let win = uilist::window_of(id).ok_or_else(|| format!("„{id}“ ist keine Element-Nummer (sie sieht aus wie desktop/1a2b)."))?;
        let w = self.ui_windows(&Full::One(win.to_string())).into_iter().find(|w| w.window == win).ok_or_else(|| format!("Das Fenster {win} gibt es nicht mehr."))?;
        let e = w.find(id).cloned().ok_or_else(|| format!("Das Element {id} gibt es nicht mehr; „ui {win}“ zeigt, was jetzt da ist."))?;
        Ok((w, e))
    }

    pub fn ui_act(&mut self, id: &str, action: &str, text: Option<&str>) -> Result<String, String> {
        self.ui_act_by(id, action, text, By::Ctl)
    }

    pub fn ui_act_by(&mut self, id: &str, action: &str, text: Option<&str>, by: By) -> Result<String, String> {
        let a = Action::of_word(action).ok_or_else(|| format!("Die Aktion „{action}“ gibt es nicht; möglich sind press, type, choose, toggle, scroll, focus, submit."))?;
        let (w, mut e) = self.ui_lookup(id)?;
        if !e.actions.contains(&a) {
            let can: Vec<&str> = e.actions.iter().map(|a| words(*a)).collect();
            return Err(format!("„{}“ kann „{action}“ nicht{}.", e.name, if can.is_empty() { String::new() } else { format!("; es kann: {}", can.join(", ")) }));
        }
        if e.has(State::Disabled) {
            return Err(format!("„{}“ ist gerade nicht bedienbar.", e.name));
        }
        agent_limits(by, &w, &e, a, text)?;
        if a == Action::Type && text.is_none_or(str::is_empty) {
            return Err("Zum Tippen fehlt der Text.".into());
        }
        // A foreign program takes any text (its EditableText, or the desktop's text input or paste, EINGABE);
        // only keys are bound to the German keyboard.
        if w.source == uilist::Source::Atspi {
            return self.ui_act_atspi(&w, &e, a, text);
        }
        // Enter is offered only on a foreign program's field (atspid); bitscomp's own fields have no such action.
        if a == Action::Submit {
            return Err(format!("„{}“ kann man nicht abschicken.", e.name));
        }
        if let Some(c) = text.filter(|_| a == Action::Type).and_then(untypable) {
            return Err(format!("Das Zeichen „{c}“ kann ich auf der deutschen Tastatur nicht tippen."));
        }
        if !e.visible {
            // Only a program window's rows scroll into sight; a bar under a sofa screen or a fullscreen window does not.
            if w.source == uilist::Source::Desktop && matches!(w.window.as_str(), crate::ui_reg::DESKTOP | crate::ui_reg::SOFA) {
                return Err(format!("„{}“ ist gerade nicht zu sehen. Es wurde nichts getan.", e.name));
            }
            e = self.ui_reveal(&w, id)?;
        }
        let (x, y) = e.centre();
        // The address bar is reached by its key, not by a click; everything else must be on top where it is clicked.
        let by_key = a == Action::Type && e.id == crate::ui_desk::address_id();
        if !by_key && !self.ui_click_at(x, y).is_some_and(|(tw, hit)| tw.window == w.window && crate::ui_stack::holds(&e, &hit.id)) {
            return Err(format!("„{}“ ist gerade verdeckt; ein Klick dort träfe etwas anderes. Es wurde nichts getan.", e.name));
        }
        match a {
            Action::Press | Action::Choose | Action::Toggle | Action::Focus | Action::Submit => self.ui_click(x, y),
            Action::Scroll => {
                let steps = text.map_or(Ok(3.0), |t| t.trim().parse::<f64>().map_err(|_| format!("„{t}“ ist keine Zahl von Schritten.")))?;
                self.handle_input(InputEvent::Motion { x, y });
                self.handle_input(InputEvent::Wheel { steps: steps.clamp(-50.0, 50.0) });
            }
            Action::Type => {
                // A field not being edited gets the text instead of what it held (End, then a BackSpace per
                // character, as a hand would); one being edited gets it at the cursor.
                if !e.has(State::Focused) {
                    if e.id == crate::ui_desk::address_id() {
                        self.ui_chord(&[29, 56], 38);
                    } else {
                        self.ui_click(x, y);
                    }
                    let held = e.value.as_deref().map_or(0, |v| v.chars().count());
                    if held > 0 {
                        self.ui_chord(&[], KEY_END);
                        for _ in 0..held {
                            self.ui_chord(&[], KEY_BACKSPACE);
                        }
                    }
                }
                self.ui_type(text.unwrap_or(""));
            }
        }
        self.ui_note(&w, &e, words(a), text.filter(|_| a == Action::Type && !e.has(State::Secret)), false);
        Ok("ok".into())
    }

    fn ui_click(&mut self, x: f64, y: f64) {
        self.handle_input(InputEvent::Motion { x, y });
        self.handle_input(InputEvent::Button { code: BTN_LEFT, pressed: true });
        self.handle_input(InputEvent::Button { code: BTN_LEFT, pressed: false });
    }

    /// Modifiers held around one key (Ctrl+Alt+L: the address bar's own key).
    pub(crate) fn ui_chord(&mut self, mods: &[u32], code: u32) {
        for m in mods {
            self.handle_input(InputEvent::Key { code: *m, pressed: true });
        }
        self.handle_input(InputEvent::Key { code, pressed: true });
        self.handle_input(InputEvent::Key { code, pressed: false });
        for m in mods.iter().rev() {
            self.handle_input(InputEvent::Key { code: *m, pressed: false });
        }
    }

    fn ui_type(&mut self, text: &str) {
        for c in text.chars() {
            let Some((code, m)) = key_of(c) else { continue };
            let held = match m {
                Mod::None => None,
                Mod::Shift => Some(SHIFT),
                Mod::AltGr => Some(ALTGR),
            };
            if let Some(h) = held {
                self.handle_input(InputEvent::Key { code: h, pressed: true });
            }
            self.handle_input(InputEvent::Key { code, pressed: true });
            self.handle_input(InputEvent::Key { code, pressed: false });
            if let Some(h) = held {
                self.handle_input(InputEvent::Key { code: h, pressed: false });
            }
        }
    }

    /// Scrolls the element's window with the wheel until the element shows (a page's row below its head): one
    /// step to learn how far a step goes, then as many as the distance needs, then single steps to settle.
    pub(crate) fn ui_reveal(&mut self, w: &uilist::Window, id: &str) -> Result<Element, String> {
        let [wx, wy, ww, wh] = w.rect;
        if ww <= 0 || wh <= 0 {
            return Err("Das Fenster ist nicht zu sehen.".into());
        }
        let (cx, cy) = (wx as f64 + ww as f64 / 2.0, wy as f64 + wh as f64 / 2.0);
        let wheel = |app: &mut App, steps: f64| {
            app.handle_input(InputEvent::Motion { x: cx, y: cy });
            app.handle_input(InputEvent::Wheel { steps });
        };
        for round in 0..REVEAL_ROUNDS {
            let (_, e) = self.ui_lookup(id)?;
            if e.visible {
                return Ok(e);
            }
            let mid = e.rect[1] as f64 + e.rect[3] as f64 / 2.0;
            let dir = if mid > cy { 1.0 } else { -1.0 };
            wheel(self, dir);
            let after = self.ui_lookup(id)?.1;
            let per = (e.rect[1] - after.rect[1]).abs() as f64;
            if per == 0.0 {
                break;
            }
            let left = ((after.rect[1] as f64 + after.rect[3] as f64 / 2.0 - cy).abs() / per).floor();
            if round == 0 && left > 1.0 {
                wheel(self, dir * (left - 1.0).min(MAX_STEPS));
            }
        }
        let (_, e) = self.ui_lookup(id)?;
        if e.visible { Ok(e) } else { Err(format!("„{}“ ist nicht zu sehen und lässt sich nicht hineinscrollen.", e.name)) }
    }

    fn ui_act_atspi(&mut self, w: &uilist::Window, e: &Element, a: Action, text: Option<&str>) -> Result<String, String> {
        // DB3: off the main loop (atspid may take seconds); the door answers when it is done.
        self.ui_act_provider(w, e, words(a), text)
    }

    /// One action into the history; `own`: Georg's hand (it goes to ctxd's habits too). A secret element that is
    /// not a field (a key of the on-screen keyboard while it types a WLAN key) is never noted: which one was
    /// pressed is the secret.
    pub(crate) fn ui_note(&self, w: &uilist::Window, e: &Element, kind: &str, text: Option<&str>, own: bool) {
        if e.has(State::Secret) && e.role != Role::Field {
            return;
        }
        let ev = uilist::Event {
            t: crate::ui_history::now_s(),
            kind: kind.into(),
            app: w.app.clone(),
            window: w.window.clone(),
            element: e.id.clone(),
            text: text.filter(|_| !e.has(State::Secret)).map(str::to_string),
            name: Some(e.name.clone()),
            by: (!own).then(|| "ctl".to_string()),
        };
        crate::ui_history::with(|h| h.push(ev, own));
    }

    /// Georg's own press (real input): the element under the pointer goes into the history before the press acts.
    /// The window on top at the pointer takes the press; within it the innermost element that acts.
    pub fn ui_note_press(&self) {
        let s = self.theme.scale;
        let (x, y) = (self.pointer.x * s, self.pointer.y * s);
        if let Some((w, e)) = self.ui_click_at(x, y) {
            self.ui_note(&w, &e, "press", None, true);
        }
    }
}
