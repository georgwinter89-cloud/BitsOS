use std::sync::mpsc::Receiver;

use mcpreg::Json;
use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::assist_client::Msg;
use crate::input::InputEvent;
use crate::input::Origin::{Injected, Physical};
use crate::state::App;

const KEY_LEFTMETA: u32 = 125;
const KEY_SPACE: u32 = 57;

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let app = App::new(d.handle(), "wy-feld-test".into(), (2560, 1440));
    (d, app)
}

/// A bitscomp whose sidebar assistant ("Mira") is ready; the lines it sends come out of the receiver.
fn with_assistant() -> (Display<App>, App, Receiver<String>) {
    let (d, mut app) = new_app();
    let (client, lines) = crate::assist_client::Client::detached();
    app.assist_client = Some(client);
    app.assist_msg(Msg::Up);
    for l in [r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":0}"#, r#"{"ok":true,"chats":[]}"#] {
        app.assist_msg(Msg::Line(Json::parse(l).unwrap()));
    }
    // EINS-B: the sidebar shows a chat (protocol v2): its questions go there.
    app.assist.chat = Some("c1".into());
    while lines.try_recv().is_ok() {}
    (d, app, lines)
}

fn type_in(app: &mut App, s: &str) {
    for c in s.chars() {
        assert!(app.feld_key(Keysym::from_char(c), &ModifiersState::default()));
    }
}

fn key(app: &mut App, sym: Keysym) {
    app.feld_key(sym, &ModifiersState::default());
}

fn kinds(app: &App) -> Vec<Kind> {
    app.feld_placed().1.iter().map(|r| r.kind).collect()
}

fn win_space(app: &mut App) {
    for (code, pressed) in [(KEY_LEFTMETA, true), (KEY_SPACE, true), (KEY_SPACE, false), (KEY_LEFTMETA, false)] {
        app.handle_input_as(InputEvent::Key { code, pressed }, Physical);
    }
}

fn hit(id: &str, name: &str) -> Hit {
    (id.into(), name.into(), format!("programm/{id}"))
}

#[test]
fn the_rows_come_in_the_order_enter_takes_them() {
    assert!(rows("  ", None, &[hit("foot", "Terminal")], Some("Mira")).is_empty(), "nothing typed: no row");
    let r = rows("term", Some(("Terminal".into(), "programm/foot".into())), &[hit("foot", "Terminal"), hit("xterm", "XTerm")], Some("Mira"));
    let k: Vec<Kind> = r.iter().map(|r| r.kind).collect();
    assert_eq!(k, [Kind::Address, Kind::Program, Kind::Ask], "the address first, its program not twice, the question last");
    assert_eq!((r[0].target.as_str(), r[1].target.as_str()), ("term", "xterm"));
    assert_eq!(r[2].title, "Frage an den Assistenten: term");
    assert_eq!((r[2].target.as_str(), r[2].detail.as_str()), ("term", "Mira"));
    let many: Vec<Hit> = (0..9).map(|i| hit(&format!("p{i}"), "P")).collect();
    let r = rows("p", None, &many, Some("Mira"));
    assert_eq!(r.len(), PROGRAMS_MAX + 1, "a few programs, the question always");
    assert_eq!(r.last().unwrap().kind, Kind::Ask);
    assert!(rows("wie spät ist es", None, &[], None).is_empty(), "no assistant in the session: no question row");
    assert_eq!(address_title("programme/hinzufuegen", None), "Programme hinzufügen");
    assert_eq!(address_title("einstellungen/aussehen", None), "Aussehen");
    assert_eq!(address_title("programm/foot", Some("Terminal")), "Terminal");
    assert_eq!(says(None), "Tippe eine Adresse, einen Programmnamen oder eine Frage.");
}

#[test]
fn win_space_opens_it_in_both_presets_and_the_library_does_not() {
    let (_d, mut app) = new_app();
    for preset in ["buehne", "classic"] {
        app.settings.set("theme.layout.preset", preset);
        app.apply_settings("theme.layout.preset");
        win_space(&mut app);
        assert!(app.feld.open && !app.lib.open, "{preset}: Win+Space opens the Feld, not the library");
        assert!(app.feld_hot().contains("feld esc at="), "{preset}");
        let bar = app.feld_hot().lines().next().unwrap().to_string();
        assert_eq!(bar.starts_with("feld at=- "), preset == "classic", "{preset}: {bar}");
        win_space(&mut app);
        assert!(!app.feld.open, "{preset}: Win+Space again closes it");
    }
}

#[test]
fn a_click_on_the_bar_opens_it_and_outside_or_esc_closes() {
    let (_d, mut app) = new_app();
    let b = app.buehne().unwrap();
    let s = app.theme.scale;
    let at = |r: crate::layout::Rect| ((r.x as f64 + r.w as f64 / 2.0) * s, (r.y as f64 + r.h as f64 / 2.0) * s);
    let click = |app: &mut App, (x, y): (f64, f64)| {
        app.handle_input_as(InputEvent::Motion { x, y }, Physical);
        app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: true }, Physical);
        app.handle_input_as(InputEvent::Button { code: crate::pointer::BTN_LEFT, pressed: false }, Physical);
    };
    click(&mut app, at(b.feld));
    assert!(app.feld.open, "a click on the Feld bar opens it");
    let below_top = 5.0 + app.chrome().top.h as f64 * s;
    click(&mut app, (5.0, below_top));
    assert!(!app.feld.open, "a click outside the panel closes it");
    click(&mut app, at(b.feld));
    assert!(app.feld.open);
    let esc = app.feld_geo().esc;
    click(&mut app, at(esc));
    assert!(!app.feld.open, "its Esc chip closes it");
    app.feld_open("test");
    type_in(&mut app, "ab");
    key(&mut app, Keysym::Escape);
    assert!(!app.feld.open && app.feld.edit.text.is_empty(), "Esc closes and forgets the text");
}

#[test]
fn addresses_programs_and_questions_resolve_in_that_order() {
    let (_d, mut app, _lines) = with_assistant();
    app.feld_open("test");
    type_in(&mut app, "einstellungen/aussehen");
    assert_eq!(kinds(&app), [Kind::Address, Kind::Ask]);
    assert_eq!(app.feld_placed().1[0].detail, "einstellungen/aussehen");
    for _ in 0.."einstellungen/aussehen".len() {
        key(&mut app, Keysym::BackSpace);
    }
    type_in(&mut app, "term");
    assert_eq!(kinds(&app), [Kind::Program, Kind::Ask], "the library's search finds Terminal");
    assert_eq!(app.feld_placed().1[0].title, "Terminal");
    // The arrows choose another row; typing makes the first one Enter's again.
    key(&mut app, Keysym::Down);
    assert_eq!(app.feld.pick(2), Some(1));
    assert!(app.feld_hot().contains("feld row 1 at=") && app.feld_hot().contains("kind=ask pick=true"), "{}", app.feld_hot());
    key(&mut app, Keysym::Down);
    assert_eq!(app.feld.pick(2), Some(0), "around");
    key(&mut app, Keysym::Up);
    type_in(&mut app, "i");
    assert_eq!(app.feld.pick(2), Some(0));
    for _ in 0..5 {
        key(&mut app, Keysym::BackSpace);
    }
    type_in(&mut app, "programme");
    assert_eq!(kinds(&app)[0], Kind::Address);
    for _ in 0..9 {
        key(&mut app, Keysym::BackSpace);
    }
    type_in(&mut app, "wie spät ist es?");
    assert_eq!(kinds(&app), [Kind::Ask], "everything else is a question");
    assert!(app.feld_state().contains("rows=1"), "{}", app.feld_state());
}

#[test]
fn enter_on_a_question_asks_the_sidebar_as_if_typed_there() {
    let (_d, mut app, lines) = with_assistant();
    app.settings.set(crate::desk::SIDEBAR_KEY, "false");
    app.apply_settings(crate::desk::SIDEBAR_KEY);
    app.input_origin = Physical;
    app.feld_open("test");
    type_in(&mut app, "wie spät ist es?");
    assert!(!app.feld.remote, "Georg's own keys");
    key(&mut app, Keysym::Return);
    assert!(!app.feld.open, "Enter closes the Feld");
    assert!(app.theme.layout.sidebar, "the sidebar opens as its ring opens it");
    assert!(app.assist.focus && app.assist.focus_physical, "its input line has the keys");
    let sent: Vec<String> = lines.try_iter().collect();
    assert!(sent.iter().any(|l| l.contains(r#""op":"ask""#) && l.contains("wie spät ist es?")), "{sent:?}");
    assert!(app.assist.edit.text.is_empty(), "submitted from the input line, which is empty again");
    // Injected keys: the question is marked as asked from outside, as in the sidebar.
    let (_d2, mut app, lines) = with_assistant();
    app.input_origin = Injected;
    app.feld_open("test");
    type_in(&mut app, "noch eine");
    assert!(app.feld.remote);
    key(&mut app, Keysym::Return);
    assert!(lines.try_iter().any(|l| l.contains("noch eine")));
    assert!(app.assist.items.iter().any(|i| matches!(i, crate::assist::Item::Note { text, .. } if text == crate::assist::REMOTE_ASK)), "marked in the list");
    assert!(!app.assist.focus_physical, "injected input gives no focus of Georg's");
}

#[test]
fn a_draft_in_the_sidebar_stays_and_a_refusal_is_the_assistants_own() {
    let (_d, mut app, lines) = with_assistant();
    app.input_origin = Physical;
    app.assist.edit.text = "mein Entwurf".into();
    app.assist.edit.cursor = 12;
    app.feld_open("test");
    type_in(&mut app, "zweite Frage");
    key(&mut app, Keysym::Return);
    assert_eq!(app.assist.edit.text, "mein Entwurf", "Georg's unsent line stays his");
    assert!(lines.try_iter().any(|l| l.contains("zweite Frage")));
    // While it answers, a second question is refused with the assistant's own line.
    app.feld_open("test");
    type_in(&mut app, "dritte");
    key(&mut app, Keysym::Return);
    assert_eq!(app.assist.note.as_ref().map(|n| n.0.as_str()), Some("Mira antwortet noch. Esc bricht ab."));
}

#[test]
fn not_over_the_sofa_or_a_dialog_and_the_library_takes_turns() {
    let (_d, mut app) = new_app();
    app.feld_open("test");
    app.library_open("test");
    assert!(app.lib.open && !app.feld.open, "the library closes the Feld");
    app.feld_open("test");
    assert!(app.feld.open && !app.lib.open, "and the other way round");
    app.feld_close("test");
    app.ws_new();
    app.desk.ws_ask = Some(crate::ws_close::Ask::test(1));
    app.feld_open("test");
    assert!(!app.feld.open, "not under the question about closing a workspace");
    app.desk.ws_ask = None;
    app.sofa_switch(crate::sofa::Way::CtlOn);
    assert!(app.sofa.listens());
    win_space(&mut app);
    assert!(!app.feld.open, "not while the sofa mode's screen listens");
}

#[test]
fn an_address_row_opens_by_open_address() {
    let (_d, mut app) = new_app();
    app.feld_open("test");
    type_in(&mut app, "sofa");
    let rows = app.feld_placed().1;
    assert_eq!((rows[0].kind, rows[0].detail.as_str()), (Kind::Address, crate::address::SOFA), "{rows:?}");
    key(&mut app, Keysym::Return);
    assert!(!app.feld.open && app.sofa.active, "the address opened (the sofa mode: no program is started here)");
    assert!(app.session.recent_addresses().iter().any(|a| a == crate::address::SOFA), "and is in the recent list as a typed one");
}
