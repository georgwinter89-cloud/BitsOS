//! DB1 (Durchblick, layer "See"): the desktop as a list. Contract C1 between the lanes: a window is
//! `{"window","app","title","pid","source","focused","rect","elements"}`, an element
//! `{"id","role","name","value","state","rect","actions","children"}`. `value` is absent for secret fields
//! everywhere (`scrub`). Rects are physical output pixels `[x, y, w, h]`, the unit of bitscomp's pointer commands.

mod ids;
mod own;
mod text;

use serde::{Deserialize, Serialize};

pub use ids::Ids;
pub use ids::{clean_id, window_of};
pub use own::{OWN_DIR, OWN_MAX, own_file, parse_own, remove_own, write_own};
pub use text::render;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Drawn by bitscomp itself (the desktop, its overlays, the settings pages).
    Desktop,
    /// One of BitsOS's own programs published its elements (the protocol file, `own`).
    Client,
    /// A foreign program's accessibility tree (contract C3).
    Atspi,
    /// Nothing but its picture is known.
    Picture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Button,
    Field,
    Text,
    Toggle,
    Choice,
    List,
    Item,
    Link,
    Tab,
    Menu,
    Menuitem,
    Heading,
    Slider,
    Image,
    Group,
    Picture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Focused,
    Disabled,
    Checked,
    Selected,
    Expanded,
    Secret,
    /// Added to C1 (EINGABE): a text field of one line (AT-SPI SINGLE_LINE) or of several (MULTI_LINE); a field
    /// without either says nothing about it.
    Singleline,
    Multiline,
    /// Added to C1 (EINGABE2): the program says this field is an address bar or a search box (its role or
    /// semantics: a search box, an input of type search, a combobox one types into with suggestions, a field in a
    /// search region).
    Search,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Press,
    Type,
    Choose,
    Toggle,
    Scroll,
    Focus,
    /// Added to C1 (EINGABE2): Enter in exactly this field (a search, an address), nothing else.
    Submit,
}

impl Action {
    pub fn of_word(w: &str) -> Option<Action> {
        Some(match w {
            "submit" | "abschicken" => Action::Submit,
            "press" => Action::Press,
            "type" => Action::Type,
            "choose" => Action::Choose,
            "toggle" => Action::Toggle,
            "scroll" => Action::Scroll,
            "focus" => Action::Focus,
            _ => return None,
        })
    }
}

fn is_true(b: &bool) -> bool {
    *b
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub id: String,
    pub role: Role,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub state: Vec<State>,
    pub rect: [i32; 4],
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Element>,
    /// Added to C1 (optional): false while the element lies outside what is shown (a scrolled page's row); it can
    /// be scrolled to, not clicked.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub visible: bool,
    /// Added to C1 (optional): a change asks first (a settings row of settingsd's class `confirm`, an action that
    /// cannot be undone); acting on it opens that question, it never skips it. Also set on `answers` buttons.
    #[serde(default, skip_serializing_if = "is_false")]
    pub confirm: bool,
    /// Added to C1 (optional): this button settles such a question ("Ja", "Ändern", "Bestätigen"): pressing it
    /// carries the change out. An agent never presses it (MCP `ui_act` refuses); Georg answers it himself.
    #[serde(default, skip_serializing_if = "is_false")]
    pub answers: bool,
}

impl Element {
    pub fn new(role: Role, name: impl Into<String>, rect: [i32; 4]) -> Element {
        Element { id: String::new(), role, name: name.into(), value: None, state: Vec::new(), rect, actions: Vec::new(), children: Vec::new(), visible: true, confirm: false, answers: false }
    }

    /// A field whose content is never shown anywhere (a key, a password).
    pub fn secret(name: impl Into<String>, rect: [i32; 4]) -> Element {
        Element::new(Role::Field, name, rect).with_state(State::Secret)
    }

    pub fn value(mut self, v: impl Into<String>) -> Element {
        if !self.state.contains(&State::Secret) {
            self.value = Some(v.into());
        }
        self
    }

    pub fn with_state(mut self, s: State) -> Element {
        if !self.state.contains(&s) {
            self.state.push(s);
        }
        if s == State::Secret {
            self.value = None;
        }
        self
    }

    /// Adds `s` when `on`.
    pub fn state_if(self, s: State, on: bool) -> Element {
        if on { self.with_state(s) } else { self }
    }

    pub fn acts(mut self, a: &[Action]) -> Element {
        for x in a {
            if !self.actions.contains(x) {
                self.actions.push(*x);
            }
        }
        self
    }

    pub fn asks(mut self, yes: bool) -> Element {
        self.confirm = yes;
        self
    }

    /// The button that answers a question yes (see `answers`).
    pub fn settles(mut self) -> Element {
        self.answers = true;
        self.confirm = true;
        self
    }

    pub fn shown(mut self, yes: bool) -> Element {
        self.visible = yes;
        self
    }

    /// A word that names the element inside its window (the base of its stable id, `Window::number`).
    pub fn key(mut self, k: impl Into<String>) -> Element {
        self.id = k.into();
        self
    }

    pub fn child(mut self, e: Element) -> Element {
        self.children.push(e);
        self
    }

    pub fn has(&self, s: State) -> bool {
        self.state.contains(&s)
    }

    pub fn centre(&self) -> (f64, f64) {
        let [x, y, w, h] = self.rect;
        (x as f64 + w as f64 / 2.0, y as f64 + h as f64 / 2.0)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        let [rx, ry, w, h] = self.rect;
        w > 0 && h > 0 && x >= rx as f64 && y >= ry as f64 && x < (rx + w) as f64 && y < (ry + h) as f64
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub window: String,
    pub app: String,
    pub title: String,
    pub pid: i64,
    pub source: Source,
    pub focused: bool,
    pub rect: [i32; 4],
    #[serde(default)]
    pub elements: Vec<Element>,
}

impl Window {
    /// Every element, depth first.
    pub fn walk(&self) -> Vec<&Element> {
        fn go<'a>(v: &'a [Element], out: &mut Vec<&'a Element>) {
            for e in v {
                out.push(e);
                go(&e.children, out);
            }
        }
        let mut out = Vec::new();
        go(&self.elements, &mut out);
        out
    }

    pub fn find(&self, id: &str) -> Option<&Element> {
        self.walk().into_iter().find(|e| e.id == id)
    }

    /// The innermost shown element under a point (the last drawn wins among siblings).
    pub fn at(&self, x: f64, y: f64) -> Option<&Element> {
        self.at_where(x, y, &|_| true)
    }

    /// The innermost shown element under a point that `keep` takes (a group around it does not hide it).
    pub fn at_where(&self, x: f64, y: f64, keep: &dyn Fn(&Element) -> bool) -> Option<&Element> {
        fn go<'a>(v: &'a [Element], x: f64, y: f64, keep: &dyn Fn(&Element) -> bool) -> Option<&'a Element> {
            for e in v.iter().rev() {
                if !e.visible {
                    continue;
                }
                if let Some(c) = go(&e.children, x, y, keep) {
                    return Some(c);
                }
                if e.contains(x, y) && keep(e) {
                    return Some(e);
                }
            }
            None
        }
        go(&self.elements, x, y, keep)
    }

    /// Gives every element its stable id under this window (`Ids`): keys are the elements' own `id` fields as the
    /// builder set them (a word that names it), else role and name.
    pub fn number(&mut self) {
        let mut ids = Ids::new(&self.window);
        fn go(v: &mut [Element], ids: &mut Ids, path: &str) {
            for e in v {
                let key = if e.id.is_empty() { format!("{path}/{:?}:{}", e.role, e.name) } else { format!("{path}/{}", e.id) };
                e.id = ids.id(&key);
                go(&mut e.children, ids, &key);
            }
        }
        go(&mut self.elements, &mut ids, "");
    }
}

/// Removes every value of a secret element (and below it), wherever the list came from.
pub fn scrub(v: &mut [Element]) {
    for e in v {
        if e.has(State::Secret) {
            e.value = None;
        }
        scrub(&mut e.children);
    }
}

/// One of Georg's actions on the desktop (`ui-history`, contract C2): what kind, in which program and window, on
/// which element, the text only where it is not secret.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unix seconds.
    pub t: u64,
    pub kind: String,
    pub app: String,
    pub window: String,
    pub element: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Added to C2 (optional): the element's name, so a reader needs no second list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Added to C2 (optional): "ctl" when the action came from the control socket or an agent, not a hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[cfg(test)]
mod tests;
