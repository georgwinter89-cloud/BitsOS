use smithay::reexports::wayland_server::Display;
use uilist::{Action, Element, Role, State};

use super::*;
use crate::input::{InputEvent, Origin};
use crate::page::{Inputs, Kind, Model, Part, Row, Section};

fn app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let a = App::new(d.handle(), "wy-ui-test".into(), (2560, 1440));
    (d, a)
}

fn desktop(app: &App) -> uilist::Window {
    app.ui_windows(&Full::One(DESKTOP.into())).into_iter().find(|w| w.window == DESKTOP).expect("desktop window")
}

fn named<'a>(w: &'a uilist::Window, role: Role, name: &str) -> &'a Element {
    w.walk().into_iter().find(|e| e.role == role && e.name == name).unwrap_or_else(|| panic!("no {role:?} {name:?} in\n{}", uilist::render(std::slice::from_ref(w))))
}

/// `X,Y` after `key=` on the first hot line that starts with `head`.
fn hot_at(app: &App, head: &str, key: &str) -> (f64, f64) {
    let hot = crate::ctl_mouse::hot(app);
    let line = hot.lines().find(|l| l.starts_with(head)).unwrap_or_else(|| panic!("no hot line {head}:\n{hot}"));
    let v = line.split_whitespace().find_map(|w| w.strip_prefix(&format!("{key}="))).expect("field");
    let (x, y) = v.split_once(',').unwrap();
    (x.parse().unwrap(), y.parse().unwrap())
}

#[test]
fn the_desktop_lists_its_bars_and_is_focused_without_windows() {
    let (_d, app) = app();
    let all = app.ui_windows(&Full::Focused);
    assert_eq!(all[0].window, DESKTOP);
    assert!(all[0].focused && all[0].source == uilist::Source::Desktop);
    let w = &all[0];
    for (role, name) in [(Role::Button, "Programme"), (Role::Field, "Adresszeile"), (Role::Button, "Startseite"), (Role::Button, "Neuer Arbeitsraum")] {
        let e = named(w, role, name);
        assert!(e.id.starts_with("desktop/"), "{}", e.id);
        let [x, y, ww, h] = e.rect;
        assert!(ww > 0 && h > 0 && x >= 0 && y >= 0 && x + ww <= 2560 && y + h <= 1440, "{name} {:?}", e.rect);
    }
    // The ids do not change between two lists.
    let again = desktop(&app);
    assert_eq!(again.walk().iter().map(|e| &e.id).collect::<Vec<_>>(), w.walk().iter().map(|e| &e.id).collect::<Vec<_>>());
}

#[test]
fn the_list_and_ctl_hot_name_the_same_points() {
    let (_d, mut app) = app();
    let w = desktop(&app);
    let (hx, hy) = hot_at(&app, "programs ", "at");
    let (x, y) = named(&w, Role::Button, "Programme").centre();
    assert!((x - hx).abs() <= 1.0 && (y - hy).abs() <= 1.0, "list {x},{y} hot {hx},{hy}");
    let (hx, hy) = hot_at(&app, "wsnew ", "at");
    let (x, y) = named(&w, Role::Button, "Neuer Arbeitsraum").centre();
    assert!((x - hx).abs() <= 1.0 && (y - hy).abs() <= 1.0);
    app.library_open("test");
    let (hx, hy) = hot_at(&app, "library add ", "at");
    let (x, y) = named(&desktop(&app), Role::Button, "Hinzufügen").centre();
    assert!((x - hx).abs() <= 1.0 && (y - hy).abs() <= 1.0);
}

#[test]
fn ui_act_presses_like_a_click_and_says_why_not() {
    let (_d, mut app) = app();
    let id = named(&desktop(&app), Role::Button, "Programme").id.clone();
    assert!(!app.lib.open);
    assert_eq!(app.ui_act(&id, "press", None).as_deref(), Ok("ok"));
    assert!(app.lib.open, "the press opened the library");
    assert_eq!(app.ui_focused_id(), DESKTOP);
    let lib = desktop(&app);
    let search = named(&lib, Role::Field, "Suche");
    assert!(search.actions.contains(&Action::Type));
    assert_eq!(app.ui_act(&id, "press", None).as_deref(), Ok("ok"));
    assert!(!app.lib.open, "and closed it again");
    for (bad, want) in [("desktop/zzzzzz", "gibt es nicht mehr"), ("nonsense", "keine Element-Nummer")] {
        let e = app.ui_act(bad, "press", None).unwrap_err();
        assert!(e.contains(want), "{e}");
    }
    assert!(app.ui_act(&id, "klick", None).unwrap_err().contains("gibt es nicht"));
    assert!(app.ui_act(&id, "type", Some("x")).unwrap_err().contains("kann „type“ nicht"));
    let addr = named(&desktop(&app), Role::Field, "Adresszeile").id.clone();
    assert!(app.ui_act(&addr, "type", Some("a😀")).unwrap_err().contains("😀"));
    // The ctl action lands in the history, marked as not Georg's own.
    let h = crate::ui_history::with(|h| h.lines(None, u64::MAX));
    assert!(h.lines().any(|l| l.contains(&format!("\"element\":\"{id}\"")) && l.contains("\"by\":\"ctl\"")), "{h}");
}

#[test]
fn typing_goes_through_the_keyboard_into_the_address_bar() {
    let (_d, mut app) = app();
    let addr = named(&desktop(&app), Role::Field, "Adresszeile").id.clone();
    assert_eq!(app.ui_act(&addr, "type", Some("einstellungen/Ü")).as_deref(), Ok("ok"));
    let now = desktop(&app);
    let f = named(&now, Role::Field, "Adresszeile");
    assert_eq!(f.value.as_deref(), Some("einstellungen/Ü"), "{f:?}");
    assert!(f.has(State::Focused));
    assert_eq!(app.ui_focused_id(), DESKTOP, "the editing bar has the keyboard");
}

#[test]
fn georgs_own_press_is_noted_with_its_element() {
    let (_d, mut app) = app();
    let e = named(&desktop(&app), Role::Button, "Neuer Arbeitsraum").clone();
    let (x, y) = e.centre();
    app.handle_input_as(InputEvent::Motion { x, y }, Origin::Physical);
    app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: true }, Origin::Physical);
    app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: false }, Origin::Physical);
    let h = crate::ui_history::with(|h| h.lines(None, u64::MAX));
    let line = h.lines().find(|l| l.contains(&format!("\"element\":\"{}\"", e.id))).unwrap_or_else(|| panic!("{h}"));
    assert!(line.contains("\"kind\":\"press\"") && line.contains("\"name\":\"Neuer Arbeitsraum\"") && !line.contains("\"by\""), "{line}");
}

#[test]
fn the_sofa_mode_is_its_own_focused_window() {
    let (_d, mut app) = app();
    app.sofa_switch(crate::sofa::Way::CtlOn);
    assert_eq!(app.ui_focused_id(), SOFA);
    let all = app.ui_windows(&Full::Focused);
    let s = all.iter().find(|w| w.window == SOFA).expect("sofa window");
    assert!(s.focused);
    assert_eq!(named(s, Role::Heading, "Start").value.as_deref(), Some("start"));
    let lib = named(s, Role::Tab, "Bibliothek").id.clone();
    assert!(named(s, Role::Button, "Schnellmenü").actions.contains(&Action::Press));
    assert_eq!(app.ui_act(&lib, "press", None).as_deref(), Ok("ok"));
    assert_eq!(app.sofa.nav.screen, crate::sofa_focus::Screen::Bibliothek, "the click went to the sofa layer");
    let s = app.ui_windows(&Full::One(SOFA.into())).into_iter().find(|w| w.window == SOFA).unwrap();
    assert!(s.walk().iter().any(|e| e.role == Role::Tab && e.name == "Spiele"), "{}", uilist::render(&[s.clone()]));
}

fn model() -> Model {
    let row = |key: &str, kind: Kind, confirm: bool| Row { key: key.into(), kind, default: String::new(), label: format!("L {key}"), short: String::new(), note: String::new(), confirm, stack: false };
    let rows = vec![
        row("a.switch", Kind::Switch, false),
        row("a.ask", Kind::Switch, true),
        row("a.key", Kind::Secret { clear: true }, false),
        row("a.mode", Kind::Choice(vec![("dark".into(), "Dunkel".into()), ("light".into(), "Hell".into())]), false),
        row("a.n", Kind::Number { min: 0, max: 10, step: 1, unit: "%".into(), word: None }, false),
    ];
    Model { sections: vec![Section::Rows("Teil".into(), 0..rows.len())], rows, ..Default::default() }
}

#[test]
fn settings_rows_carry_label_value_kind_and_confirm_never_a_secret() {
    let (_d, app) = app();
    let m = model();
    let lay = crate::page::layout(Rect { x: 0, y: 0, w: 1400, h: 1300 }, &m, &Inputs { favourites: 0, scroll: 0 });
    let typed = |p: Part| (p == Part::Field(2)).then(|| "getippt".to_string());
    let rows: Vec<Element> = m.rows.iter().enumerate().map(|(i, r)| app.ui_row(&lay, i, r, if i == 2 { "sk-geheim-123" } else if i == 3 { "light" } else if i == 4 { "7" } else { "true" }, &typed, false)).collect();
    assert_eq!((rows[0].role, rows[0].value.as_deref(), rows[0].has(State::Checked), rows[0].confirm), (Role::Toggle, Some("an"), true, false));
    assert!(rows[1].confirm, "a confirm row says that it asks");
    let sw = lay.part(crate::page::Part::Switch(0)).unwrap();
    assert_eq!(rows[0].rect, app.ui_px(sw), "the toggle is where the switch is drawn");
    assert!(rows[2].has(State::Secret) && rows[2].value.is_none());
    let mut w = uilist::Window { window: "w1".into(), app: "x".into(), title: "x".into(), pid: 1, source: uilist::Source::Desktop, focused: true, rect: [0, 0, 1, 1], elements: rows.clone() };
    w.number();
    let json = serde_json::to_string(&w).unwrap();
    assert!(!json.contains("sk-geheim") && !json.contains("getippt"), "{json}");
    assert_eq!(rows[2].children[0].name, "Löschen");
    assert_eq!(rows[3].value.as_deref(), Some("Hell"));
    assert!(rows[3].children.iter().any(|c| c.name == "Hell" && c.has(State::Selected)));
    assert_eq!(rows[4].value.as_deref(), Some("7 %"));
    assert_eq!(rows[4].children.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["weniger", "Zahl", "mehr"]);
    // While the page's dialog is open its rows take no click.
    let d = app.ui_row(&lay, 0, &m.rows[0], "false", &|_| None, true);
    assert!(d.has(State::Disabled) && !d.has(State::Checked));
}

#[test]
fn a_secret_key_of_settingsd_never_carries_its_value() {
    let (_d, app) = app();
    let row = Row { key: "net.secret".into(), kind: Kind::Fixed(crate::page_schema::SECRET_NOTE.into()), default: "werkswert".into(), label: "Geheim".into(), short: String::new(), note: String::new(), confirm: false, stack: false };
    let m = Model { sections: vec![Section::Rows("Teil".into(), 0..1)], rows: vec![row], ..Default::default() };
    let lay = crate::page::layout(Rect { x: 0, y: 0, w: 1400, h: 600 }, &m, &Inputs { favourites: 0, scroll: 0 });
    let e = app.ui_row(&lay, 0, &m.rows[0], "klartext", &|_| None, false);
    assert!(e.has(State::Secret) && e.value.is_none() && e.actions.is_empty(), "{e:?}");
    assert!(!serde_json::to_string(&e).unwrap().contains("klartext"));
}

#[test]
fn a_field_being_edited_shows_what_is_typed_and_has_the_focus() {
    let (_d, app) = app();
    let row = |key: &str, kind: Kind| Row { key: key.into(), kind, default: String::new(), label: key.into(), short: String::new(), note: String::new(), confirm: false, stack: false };
    let m = Model { sections: vec![Section::Rows("Teil".into(), 0..2)], rows: vec![row("theme.accent", Kind::Accent), row("a.n", Kind::Number { min: 0, max: 99, step: 1, unit: String::new(), word: None })], ..Default::default() };
    let lay = crate::page::layout(Rect { x: 0, y: 0, w: 1400, h: 900 }, &m, &Inputs { favourites: 0, scroll: 0 });
    let typed = |p: Part| match p {
        Part::Hex => Some("#12".to_string()),
        Part::Field(1) => Some("4".to_string()),
        _ => None,
    };
    let accent = app.ui_row(&lay, 0, &m.rows[0], "#336699", &typed, false);
    let hex = accent.children.iter().find(|c| c.name == "Farbe als Hex").expect("hex field");
    assert_eq!((hex.value.as_deref(), hex.has(State::Focused)), (Some("#12"), true));
    let n = app.ui_row(&lay, 1, &m.rows[1], "40", &typed, false);
    let f = n.children.iter().find(|c| c.name == "Zahl").expect("number field");
    assert_eq!((f.value.as_deref(), f.has(State::Focused)), (Some("4"), true));
    let idle = app.ui_row(&lay, 0, &m.rows[0], "#336699", &|_| None, false);
    let hex = idle.children.iter().find(|c| c.name == "Farbe als Hex").unwrap();
    assert_eq!((hex.value.as_deref(), hex.has(State::Focused)), (Some("#336699"), false));
}

#[test]
fn a_wlan_key_typed_on_the_sofa_keyboard_leaves_no_trace() {
    let (_d, mut app) = app();
    app.sofa_switch(crate::sofa::Way::CtlOn);
    app.sofa_search_open(crate::sofa_search::Target::Setting { key: "net.wifi.psk".into(), title: "WLAN-Passwort".into(), secret: true }, "");
    let sofa = |app: &App| app.ui_windows(&Full::One(SOFA.into())).into_iter().find(|w| w.window == SOFA).unwrap();
    let s = sofa(&app);
    let a = named(&s, Role::Button, "a").clone();
    let b = named(&s, Role::Button, "b").id.clone();
    assert!(a.has(State::Secret), "a key of a secret entry is secret: {a:?}");
    let field = named(&s, Role::Field, "Eingabe");
    assert!(field.has(State::Secret) && field.value.is_none());
    assert_eq!(field.rect, { let f = app.sofa_frame().r(crate::sofa_layout::FIELD); [f.x, f.y, f.w, f.h] }, "the field is where it is drawn");
    // Georg's hand on "a", a script on "b": both type, neither is noted anywhere.
    let (x, y) = a.centre();
    app.handle_input_as(InputEvent::Motion { x, y }, Origin::Physical);
    app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: true }, Origin::Physical);
    app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: false }, Origin::Physical);
    assert_eq!(app.ui_act(&b, "press", None).as_deref(), Ok("ok"));
    assert_eq!(app.sofa.search.field.text, "ab", "the presses typed");
    let h = crate::ui_history::with(|h| h.lines(None, u64::MAX));
    assert!(!h.contains(&a.id) && !h.contains(&b), "{h}");
    let s = sofa(&app);
    assert!(s.walk().iter().filter(|e| e.has(State::Secret) && e.role == Role::Button).all(|e| !e.has(State::Focused)), "the focus never shows which key");
    assert!(!uilist::render(std::slice::from_ref(&s)).contains("ab\""));
}

#[test]
fn an_agent_never_answers_a_question_but_a_script_may() {
    let (_d, mut app) = app();
    app.desk.ws_ask = Some(crate::ws_close::Ask::test(1));
    let w = desktop(&app);
    let yes = w.walk().into_iter().find(|e| e.answers).expect("the question's yes").clone();
    assert_eq!(yes.name, "Schließen");
    let no = named(&w, Role::Button, "Abbrechen").id.clone();
    assert!(yes.answers && yes.confirm, "{yes:?}");
    let r = app.ui_act_by(&yes.id, "press", None, crate::ui_act::By::Mcp);
    assert!(r.as_ref().is_err_and(|e| e.contains("Georg selbst")), "{r:?}");
    assert!(app.desk.ws_ask.is_some(), "nothing was pressed");
    assert_eq!(app.ui_act_by(&no, "press", None, crate::ui_act::By::Mcp).as_deref(), Ok("ok"));
    assert!(app.desk.ws_ask.is_none(), "Abbrechen is the agent's to press");
}

#[test]
fn under_a_sofa_screen_the_bars_are_neither_shown_nor_pressed() {
    let (_d, mut app) = app();
    let prog = named(&desktop(&app), Role::Button, "Programme").clone();
    app.sofa_switch(crate::sofa::Way::CtlOn);
    assert!(desktop(&app).walk().iter().find(|e| e.id == prog.id).is_some_and(|e| !e.visible), "the top bar is not drawn under the sofa");
    let r = app.ui_act(&prog.id, "press", None);
    assert!(r.as_ref().is_err_and(|e| e.contains("nicht zu sehen")), "{r:?}");
    assert!(!app.lib.open, "nothing behind the sofa was clicked");
    // Georg's own press there belongs to the sofa (or to nothing), never to the bar under it.
    let (x, y) = prog.centre();
    app.handle_input_as(InputEvent::Motion { x, y }, Origin::Physical);
    app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: true }, Origin::Physical);
    app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: false }, Origin::Physical);
    let h = crate::ui_history::with(|h| h.lines(None, u64::MAX));
    assert!(!h.lines().any(|l| l.contains(&prog.id) && !l.contains("\"by\"")), "{h}");
}

#[test]
fn an_agent_never_presses_a_secret_key_types_a_secret_or_changes_a_confirm_setting() {
    let (_d, mut app) = app();
    app.sofa_switch(crate::sofa::Way::CtlOn);
    app.sofa_search_open(crate::sofa_search::Target::Setting { key: "net.wifi.psk".into(), title: "WLAN-Passwort".into(), secret: true }, "");
    let s = app.ui_windows(&Full::One(SOFA.into())).into_iter().find(|w| w.window == SOFA).unwrap();
    let key = named(&s, Role::Button, "a").id.clone();
    for action in ["press", "focus"] {
        let r = app.ui_act_by(&key, action, None, crate::ui_act::By::Mcp);
        assert!(r.as_ref().is_err_and(|e| e.contains("geheim")), "{action}: {r:?}");
    }
    assert!(app.sofa.search.field.text.is_empty(), "nothing was typed");
    assert_eq!(app.ui_act(&key, "press", None).as_deref(), Ok("ok"), "a script on this machine still types (it never sees the key)");
    // A settings row that asks first, and the "mehr" of one, are Georg's (the check `ui_act_by` makes for an agent).
    let m = model();
    let lay = crate::page::layout(Rect { x: 0, y: 0, w: 1400, h: 1300 }, &m, &Inputs { favourites: 0, scroll: 0 });
    let rows: Vec<Element> = m.rows.iter().enumerate().map(|(i, r)| app.ui_row(&lay, i, r, "true", &|_| None, false)).collect();
    let mut w = uilist::Window { window: "w1".into(), app: "einstellungen".into(), title: "Einstellungen".into(), pid: 1, source: uilist::Source::Desktop, focused: true, rect: [0, 0, 1, 1], elements: rows };
    w.number();
    let ask = w.walk().into_iter().find(|e| e.name == "L a.ask").unwrap().clone();
    let r = crate::ui_act::agent_limits(crate::ui_act::By::Mcp, &w, &ask, Action::Toggle, None);
    assert!(r.as_ref().is_err_and(|e| e.contains("nur Georg")), "{r:?}");
    assert!(crate::ui_act::agent_limits(crate::ui_act::By::Ctl, &w, &ask, Action::Toggle, None).is_ok(), "a script on this machine still may (the page asks)");
    let plain = w.walk().into_iter().find(|e| e.name == "L a.switch").unwrap().clone();
    assert!(crate::ui_act::agent_limits(crate::ui_act::By::Mcp, &w, &plain, Action::Toggle, None).is_ok());
}
