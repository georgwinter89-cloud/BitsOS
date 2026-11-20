//! A read `Node` tree into the element list (contract C1): roles, names, values, states, rectangles on the desktop,
//! actions. Layout-only containers are left out and their children lifted; a password field never has a value.

use crate::atspi::{has, role, state};
use crate::ids::WinIds;
use crate::read::Node;
use serde_json::{Map, Value, json};

/// Where the program's surface (its buffer, client-side shadows included: bitscomp's `window_origin`) starts on
/// the desktop, and the factor from the program's own pixels to the desktop's (1 unless the caller gave the
/// surface's size and the program counts in other pixels, e.g. physical ones under a scaled desktop).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Origin {
    pub x: i32,
    pub y: i32,
    pub sx: f64,
    pub sy: f64,
}

impl Default for Origin {
    fn default() -> Self {
        Origin::at(0, 0)
    }
}

impl Origin {
    pub fn at(x: i32, y: i32) -> Self {
        Origin { x, y, sx: 1.0, sy: 1.0 }
    }

    /// Scaled so that the program's `own` window size [w,h] becomes the desktop's `size`; a difference under 2 %
    /// is rounding, not a scale.
    pub fn scaled(self, own: [i32; 2], size: [i32; 2]) -> Self {
        let f = |o: i32, s: i32| {
            let k = if o > 0 && s > 0 { f64::from(s) / f64::from(o) } else { 1.0 };
            if (k - 1.0).abs() < 0.02 { 1.0 } else { k }
        };
        Origin { sx: f(own[0], size[0]), sy: f(own[1], size[1]), ..self }
    }

    /// A rectangle in the program's window coordinates on the desktop.
    pub fn place(&self, [x, y, w, h]: [i32; 4]) -> [i32; 4] {
        let s = |v: i32, k: f64| (f64::from(v) * k).round() as i32;
        [self.x + s(x, self.sx), self.y + s(y, self.sy), s(w, self.sx), s(h, self.sy)]
    }
}

pub struct Ctx<'a> {
    pub win: &'a str,
    pub origin: Origin,
    pub ids: &'a mut WinIds,
    /// Inside a search region (a landmark whose role is `search`): its fields are search boxes.
    pub search: bool,
    /// Inside a choice: its options are listed although the closed popup hides them (`choose` names them).
    pub options: bool,
    /// The program marks what is visible (its window says VISIBLE): only then does a missing VISIBLE hide.
    pub marks_visible: bool,
}

impl<'a> Ctx<'a> {
    pub fn new(win: &'a str, origin: Origin, ids: &'a mut WinIds, marks_visible: bool) -> Self {
        Ctx { win, origin, ids, search: false, options: false, marks_visible }
    }
}

/// The C1 role, or None for a container that only lays things out.
pub fn c1_role(n: &Node) -> Option<&'static str> {
    let named = !n.name.trim().is_empty();
    Some(match n.role {
        role::PUSH_BUTTON | role::PUSH_BUTTON_MENU | role::ARROW => "button",
        role::CHECK_BOX | role::TOGGLE_BUTTON | role::RADIO_BUTTON | role::SWITCH => "toggle",
        role::ENTRY | role::PASSWORD_TEXT | role::SPIN_BUTTON | role::AUTOCOMPLETE | role::EDITBAR | role::DATE_EDITOR => "field",
        role::TEXT | role::TERMINAL if has(n.states, state::EDITABLE) => "field",
        role::TEXT | role::TERMINAL | role::LABEL | role::PARAGRAPH | role::STATIC | role::CAPTION | role::TOOL_TIP | role::STATUS_BAR => "text",
        role::COMBO_BOX => "choice",
        role::LIST | role::LIST_BOX | role::TREE | role::TABLE | role::TREE_TABLE | role::DESCRIPTION_LIST => "list",
        role::LIST_ITEM | role::TREE_ITEM | role::TABLE_CELL => "item",
        role::LINK => "link",
        role::PAGE_TAB => "tab",
        role::MENU | role::MENU_BAR | role::POPUP_MENU => "menu",
        role::MENU_ITEM | role::CHECK_MENU_ITEM | role::RADIO_MENU_ITEM | role::TEAROFF_MENU_ITEM => "menuitem",
        role::HEADING => "heading",
        role::SLIDER | role::DIAL | role::SCROLL_BAR | role::PROGRESS_BAR | role::LEVEL_BAR => "slider",
        role::IMAGE | role::ICON | role::IMAGE_MAP => "image",
        role::CANVAS | role::DRAWING_AREA | role::VIDEO | role::EMBEDDED => "picture",
        role::DIALOG | role::ALERT | role::NOTIFICATION | role::PAGE_TAB_LIST | role::TOOL_BAR => "group",
        role::FORM | role::GROUPING | role::LANDMARK | role::ARTICLE | role::DOCUMENT_FRAME | role::DOCUMENT_WEB | role::DOCUMENT_TEXT
        | role::DOCUMENT_SPREADSHEET | role::DOCUMENT_PRESENTATION | role::DOCUMENT_EMAIL
            if named =>
        {
            "group"
        }
        _ => return None,
    })
}

fn is_toggle_item(n: &Node) -> bool {
    matches!(n.role, role::CHECK_MENU_ITEM | role::RADIO_MENU_ITEM)
}

fn actionable(r: &str) -> bool {
    matches!(r, "button" | "toggle" | "field" | "choice" | "link" | "tab" | "menuitem" | "item" | "slider")
}

/// The first descendant that is selected (the chosen option of a choice).
fn selected_name(n: &Node) -> Option<String> {
    n.children.iter().find_map(|c| {
        if has(c.states, state::SELECTED) && !c.name.is_empty() { Some(c.name.clone()) } else { selected_name(c) }
    })
}

fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 { format!("{}", v as i64) } else { format!("{v}") }
}

/// A field or an editable choice (a combobox one types into, as a web page's search box).
fn writable(n: &Node) -> bool {
    has(n.states, state::EDITABLE) && !has(n.states, state::READ_ONLY)
}

fn texty(n: &Node, r: &str) -> bool {
    r == "field" || (r == "choice" && writable(n))
}

/// The words of an object attribute that holds several (`xml-roles` may say "combobox searchbox").
fn attr_has(n: &Node, key: &str, words: &[&str]) -> bool {
    n.attr(key).is_some_and(|v| v.split_whitespace().any(|w| words.contains(&w.to_lowercase().as_str())))
}

/// EINGABE2: an address bar or a search box by what the program says it is (never by its name: bitscomp adds
/// that as a second signal): ARIA's searchbox, an input of type search, a combobox one types into with
/// suggestions, AT-SPI's autocomplete role, a field inside a search region.
pub fn search_box(n: &Node, in_search: bool) -> bool {
    let suggests = attr_has(n, "autocomplete", &["list", "both", "inline"]);
    attr_has(n, "xml-roles", &["searchbox", "search"])
        || attr_has(n, "text-input-type", &["search"])
        || n.role == role::AUTOCOMPLETE
        || (suggests && (n.role == role::COMBO_BOX || attr_has(n, "xml-roles", &["combobox"])))
        || in_search
}

/// F (EINGABE2): an element one could act on that the program says is not visible (CSS hidden, a closed
/// dropdown's copy of a button) or that has no size is not offered; what is merely scrolled out of the window stays
/// (`scroll` reaches it), and so does the focused element.
fn hidden(n: &Node, r: &str, cx: &Ctx<'_>) -> bool {
    if !actionable(r) || cx.options || has(n.states, state::FOCUSED) {
        return false;
    }
    let no_size = n.rect.is_some_and(|[_, _, w, h]| w <= 0 || h <= 0);
    (cx.marks_visible && !has(n.states, state::VISIBLE)) || no_size
}

fn states(n: &Node, r: &str, secret: bool, in_search: bool) -> Vec<&'static str> {
    let mut s = Vec::new();
    let mut add = |c: bool, w| {
        if c {
            s.push(w)
        }
    };
    add(has(n.states, state::FOCUSED), "focused");
    add(actionable(r) && !has(n.states, state::ENABLED) && !has(n.states, state::SENSITIVE), "disabled");
    add(has(n.states, state::CHECKED) || (n.role == role::TOGGLE_BUTTON && has(n.states, state::PRESSED)), "checked");
    add(has(n.states, state::SELECTED), "selected");
    add(has(n.states, state::EXPANDED), "expanded");
    add(secret, "secret");
    // Durchblick's rules for typing tell one line from several (EINGABE).
    add(texty(n, r) && has(n.states, state::SINGLE_LINE), "singleline");
    add(texty(n, r) && has(n.states, state::MULTI_LINE), "multiline");
    add(texty(n, r) && writable(n) && search_box(n, in_search), "search");
    s
}

pub fn actions(n: &Node, r: &str, parent_selects: bool) -> Vec<&'static str> {
    let mut a = Vec::new();
    let can_act = !n.actions.is_empty();
    let writable = writable(n);
    if can_act && (r == "toggle" || is_toggle_item(n)) {
        a.push("toggle");
    } else if can_act && matches!(r, "button" | "link" | "menuitem" | "tab" | "item" | "image" | "choice" | "menu") {
        a.push("press");
    }
    // EINGABE: a field without EditableText (Chromium's, Brave's) is written by the desktop (bitscomp's own
    // text input or a paste only that window sees); a terminal is never written into that way.
    let by_desktop = writable && n.role != role::TERMINAL && matches!(r, "field" | "choice");
    if (r == "field" && n.has_iface("EditableText") && writable)
        || by_desktop
        || (r == "slider" && n.has_iface("Value") && matches!(n.role, role::SLIDER | role::DIAL | role::SPIN_BUTTON))
    {
        a.push("type");
    }
    // EINGABE2: Enter in exactly this field (bitscomp presses it once the field has the keyboard).
    if writable && !matches!(n.role, role::TERMINAL | role::PASSWORD_TEXT) && matches!(r, "field" | "choice") {
        a.push("submit");
    }
    if r == "choice" || (r == "list" && n.has_iface("Selection")) || (matches!(r, "item" | "menuitem") && parent_selects) {
        a.push("choose");
    }
    if has(n.states, state::FOCUSABLE) && n.has_iface("Component") {
        a.push("focus");
    }
    a
}

/// The elements `n` stands for: itself with its children, or (a layout container) its children.
pub fn elements(n: &Node, cx: &mut Ctx<'_>, parent: &str, parent_selects: bool) -> Vec<Value> {
    cx.ids.parent_of(&n.path, parent);
    let selects = n.has_iface("Selection");
    let was = (cx.search, cx.options);
    cx.search |= matches!(n.role, role::LANDMARK | role::FORM) && attr_has(n, "xml-roles", &["search"]);
    let out = element(n, cx, parent_selects, selects);
    (cx.search, cx.options) = was;
    out
}

fn element(n: &Node, cx: &mut Ctx<'_>, parent_selects: bool, selects: bool) -> Vec<Value> {
    let lift = |cx: &mut Ctx<'_>| n.children.iter().flat_map(|c| elements(c, cx, &n.path, selects)).collect();
    let r = match c1_role(n) {
        Some(r) if !hidden(n, r, cx) => r,
        _ => return lift(cx),
    };
    let secret = n.role == role::PASSWORD_TEXT;
    let text = if secret { None } else { n.text.clone().filter(|t| !t.is_empty()) };
    let mut name = n.name.trim().to_string();
    if name.is_empty() && matches!(r, "text" | "heading" | "link") {
        name = text.clone().unwrap_or_default();
    }
    if r == "text" && name.is_empty() && n.children.is_empty() {
        return Vec::new();
    }
    let value = match r {
        _ if secret => None,
        // Only a text that was read: a field whose text is unknown may hold anything (an empty one says "").
        "field" => n.text.clone(),
        // One can type into it: only what was read (a chosen suggestion's name is not what the box holds).
        "choice" if writable(n) => n.text.clone(),
        "choice" => selected_name(n).or(text),
        "slider" => n.value.map(fmt_num),
        _ => None,
    };
    let mut e = Map::new();
    e.insert("id".into(), json!(format!("{}/{}", cx.win, cx.ids.id(&n.path))));
    e.insert("role".into(), json!(r));
    e.insert("name".into(), json!(name));
    if let Some(v) = value {
        e.insert("value".into(), json!(v));
    }
    e.insert("state".into(), json!(states(n, r, secret, cx.search)));
    e.insert("rect".into(), json!(cx.origin.place(n.rect.unwrap_or_default())));
    e.insert("actions".into(), json!(actions(n, r, parent_selects)));
    cx.options |= r == "choice";
    let kids: Vec<Value> = n.children.iter().flat_map(|c| elements(c, cx, &n.path, selects)).collect();
    e.insert("children".into(), Value::Array(kids));
    vec![Value::Object(e)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atspi::state;

    fn node(role: u32, name: &str, states: &[u32]) -> Node {
        Node { path: format!("/{name}"), role, name: name.into(), states: states.iter().fold(0, |m, s| m | 1 << s), ..Node::default() }
    }

    fn map(n: &Node) -> Vec<Value> {
        let mut ids = WinIds::default();
        let mut cx = Ctx::new("w", Origin::at(5, 7), &mut ids, false);
        elements(n, &mut cx, "/", false)
    }

    #[test]
    fn a_password_never_has_a_value_and_says_secret() {
        let mut p = node(role::PASSWORD_TEXT, "Passwort", &[state::EDITABLE, state::ENABLED]);
        p.text = Some("geheim".into());
        p.ifaces = vec!["org.a11y.atspi.EditableText".into()];
        let e = &map(&p)[0];
        assert!(e.get("value").is_none());
        assert_eq!(e["state"], json!(["secret"]));
        assert_eq!(e["actions"], json!(["type"]));
        assert!(!e.to_string().contains("geheim"));
    }

    #[test]
    fn layout_containers_are_lifted_and_rects_move_to_the_desktop() {
        let mut panel = node(39, "", &[state::ENABLED]);
        let mut b = node(role::PUSH_BUTTON, "OK", &[state::ENABLED]);
        b.rect = Some([1, 2, 3, 4]);
        b.actions = vec!["click".into()];
        panel.children = vec![b];
        let v = map(&panel);
        assert_eq!(v.len(), 1);
        assert_eq!((v[0]["role"].as_str(), v[0]["id"].as_str()), (Some("button"), Some("w/1")));
        assert_eq!(v[0]["rect"], json!([6, 9, 3, 4]));
        assert_eq!(v[0]["actions"], json!(["press"]));
    }

    #[test]
    fn a_scaled_program_is_placed_in_desktop_pixels() {
        let o = Origin::at(100, 50).scaled([1000, 750], [800, 600]);
        assert_eq!(o.place([100, 10, 200, 20]), [180, 58, 160, 16]);
        assert_eq!(Origin::at(1, 2).scaled([800, 600], [805, 598]).place([10, 10, 5, 5]), [11, 12, 5, 5], "rounding is no scale");
        assert_eq!(Origin::at(0, 0).scaled([0, 0], [800, 600]), Origin::at(0, 0), "no size of its own: unscaled");
    }

    #[test]
    fn states_and_values() {
        let off = node(role::PUSH_BUTTON, "Aus", &[]);
        assert_eq!(map(&off)[0]["state"], json!(["disabled"]));
        let label = node(role::LABEL, "", &[]);
        assert!(map(&label).is_empty(), "an empty label says nothing");
        let mut s = node(role::SLIDER, "Laut", &[state::ENABLED]);
        s.value = Some(12.5);
        assert_eq!(map(&s)[0]["value"], "12.5");
        let mut t = node(role::TOGGLE_BUTTON, "Fett", &[state::ENABLED, state::PRESSED]);
        t.actions = vec!["press".into()];
        assert_eq!(map(&t)[0]["state"], json!(["checked"]));
        assert_eq!(map(&t)[0]["actions"], json!(["toggle"]));
        let mut c = node(role::COMBO_BOX, "Farbe", &[state::ENABLED]);
        let mut menu = node(role::POPUP_MENU, "", &[]);
        menu.children = vec![node(role::MENU_ITEM, "Rot", &[]), node(role::MENU_ITEM, "Blau", &[state::SELECTED])];
        c.children = vec![menu];
        assert_eq!(map(&c)[0]["value"], "Blau");
        let doc = node(role::DOCUMENT_WEB, "", &[]);
        assert!(c1_role(&doc).is_none(), "an unnamed document is only layout");
        assert_eq!(c1_role(&node(role::DOCUMENT_WEB, "Seite", &[])), Some("group"));
    }

    #[test]
    fn a_chromium_field_without_editable_text_offers_type_by_the_desktop() {
        let mut url = node(role::ENTRY, "Adress- und Suchleiste", &[state::EDITABLE, state::FOCUSABLE, state::ENABLED, state::SINGLE_LINE]);
        url.ifaces = vec!["org.a11y.atspi.Component".into(), "org.a11y.atspi.Text".into()];
        url.text = Some("https://example.org/".into());
        let e = &map(&url)[0];
        assert_eq!(e["actions"], json!(["type", "submit", "focus"]), "type although EditableText is missing");
        assert_eq!(e["state"], json!(["singleline"]));
        assert_eq!(e["value"], "https://example.org/");
        let mut text = node(role::TEXT, "Kommentar", &[state::EDITABLE, state::FOCUSABLE, state::ENABLED, state::MULTI_LINE]);
        text.ifaces = url.ifaces.clone();
        text.text = Some(String::new());
        let e = &map(&text)[0];
        assert_eq!((e["role"].as_str(), e["value"].as_str()), (Some("field"), Some("")), "read and empty says \"\"");
        assert_eq!(e["state"], json!(["multiline"]));
        assert!(e["actions"].as_array().unwrap().contains(&json!("type")));
        let mut unread = node(role::ENTRY, "Ohne Text", &[state::EDITABLE, state::ENABLED]);
        unread.ifaces = vec!["org.a11y.atspi.Component".into()];
        assert!(map(&unread)[0].get("value").is_none(), "a text that was not read is unknown, not empty");
        let ro = node(role::ENTRY, "Nur lesen", &[state::EDITABLE, state::READ_ONLY, state::ENABLED]);
        assert!(!map(&ro)[0]["actions"].as_array().unwrap().contains(&json!("type")), "read-only: no type");
        let term = node(role::TERMINAL, "Terminal", &[state::EDITABLE, state::ENABLED]);
        assert!(!map(&term)[0]["actions"].as_array().unwrap().contains(&json!("type")), "a terminal is never written by the desktop");
    }

    #[test]
    fn an_editable_combobox_offers_type() {
        let mut c = node(role::COMBO_BOX, "Suche", &[state::EDITABLE, state::FOCUSABLE, state::ENABLED, state::SINGLE_LINE]);
        c.ifaces = vec!["org.a11y.atspi.Component".into(), "org.a11y.atspi.Text".into(), "org.a11y.atspi.Action".into()];
        c.actions = vec!["click".into()];
        c.text = Some("Wetter".into());
        let e = &map(&c)[0];
        assert_eq!(e["role"], "choice");
        assert_eq!(e["actions"], json!(["press", "type", "submit", "choose", "focus"]), "Google's search box: press, type, submit, choose, focus");
        assert_eq!(e["value"], "Wetter", "what was typed in it, not a chosen option");
        assert_eq!(e["state"], json!(["singleline"]));
        let mut unread = node(role::COMBO_BOX, "Suche", &[state::EDITABLE, state::FOCUSABLE, state::ENABLED, state::MULTI_LINE]);
        let mut hint = node(role::LIST_ITEM, "wetter morgen", &[state::SELECTED, state::ENABLED]);
        hint.path = "/Suche/0".into();
        unread.children = vec![hint];
        assert!(map(&unread)[0].get("value").is_none(), "its text not read: unknown, not the chosen suggestion's name");
        let mut plain = node(role::COMBO_BOX, "Farbe", &[state::FOCUSABLE, state::ENABLED]);
        plain.actions = vec!["click".into()];
        assert!(!map(&plain)[0]["actions"].as_array().unwrap().contains(&json!("type")), "a combobox one cannot type into");
    }

    #[test]
    fn search_boxes_by_their_semantics_and_hidden_copies_are_not_offered() {
        let f = [state::EDITABLE, state::FOCUSABLE, state::ENABLED, state::SHOWING, state::VISIBLE, state::SINGLE_LINE];
        let mut google = node(role::COMBO_BOX, "Suche", &f);
        google.attrs = vec![("autocomplete".into(), "both".into()), ("xml-roles".into(), "combobox".into())];
        let mut typed = node(role::ENTRY, "Begriff", &f);
        typed.attrs = vec![("text-input-type".into(), "search".into())];
        let plain = node(role::ENTRY, "Kommentar", &f);
        let mut region = node(role::LANDMARK, "", &[state::SHOWING, state::VISIBLE]);
        region.attrs = vec![("xml-roles".into(), "search".into())];
        let mut inside = node(role::ENTRY, "Wort", &f);
        inside.path = "/in".into();
        region.children = vec![inside];
        let mut ids = WinIds::default();
        let mut cx = Ctx::new("w", Origin::at(0, 0), &mut ids, true);
        let mut all = Vec::new();
        for n in [&google, &typed, &plain, &region] {
            all.extend(elements(n, &mut cx, "/", false));
        }
        let st = |i: usize| all[i]["state"].as_array().unwrap().contains(&json!("search"));
        assert!(st(0) && st(1) && !st(2) && st(3), "combobox with suggestions, type search, plain field, field in a search region: {all:?}");
        let mut copy = node(role::PUSH_BUTTON, "Google Suche", &[state::SHOWING, state::ENABLED]);
        copy.actions = vec!["click".into()];
        copy.rect = Some([0, 0, 0, 0]);
        let mut seen = copy.clone();
        seen.states |= 1 << state::VISIBLE;
        seen.rect = Some([40, 170, 120, 28]);
        let mut far = seen.clone();
        far.rect = Some([40, 4000, 120, 28]);
        let mut focused = copy.clone();
        focused.states |= 1 << state::FOCUSED;
        let mut ids = WinIds::default();
        let mut cx = Ctx::new("w", Origin::at(0, 0), &mut ids, true);
        assert!(elements(&copy, &mut cx, "/", false).is_empty(), "not visible and no size: not offered");
        assert_eq!(elements(&seen, &mut cx, "/", false).len(), 1);
        assert_eq!(elements(&far, &mut cx, "/", false).len(), 1, "scrolled out of the window: scroll reaches it");
        assert_eq!(elements(&focused, &mut cx, "/", false).len(), 1, "the focused element always stays");
        let mut ids = WinIds::default();
        let mut cx = Ctx::new("w", Origin::at(0, 0), &mut ids, false);
        let mut unmarked = seen.clone();
        unmarked.states &= !(1 << state::VISIBLE);
        assert_eq!(elements(&unmarked, &mut cx, "/", false).len(), 1, "a program that never says VISIBLE hides nothing that way");
    }
}
