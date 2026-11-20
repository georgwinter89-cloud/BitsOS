use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::reexports::wayland_server::Display;

use super::*;

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let app = App::new(d.handle(), "wy-library-add-test".into(), (2560, 1440));
    (d, app)
}

fn key(app: &mut App, sym: Keysym, shift: bool) {
    let mods = ModifiersState { shift, ..ModifiersState::default() };
    app.library_key(sym, &mods);
}

fn tile(app: &App) -> (Option<Button>, usize) {
    (app.lib.button, app.lib.focus.as_ref().map_or(usize::MAX, |f| f.0))
}

#[test]
fn the_keys_reach_the_head_and_come_back_to_the_tiles() {
    let (_d, mut app) = new_app();
    app.library_open("test");
    let rows = app.library_rows().len();
    assert!(rows >= 2 && app.library_head().updates.is_some(), "two rows and both buttons at 2560x1440");
    // Up from the first line: "Programme hinzufügen"; Right: "Aktualisierungen"; Down: the first tile again.
    key(&mut app, Keysym::Up, false);
    assert_eq!(app.lib.button, Some(Button::Add));
    key(&mut app, Keysym::Right, false);
    assert_eq!(app.lib.button, Some(Button::Updates));
    key(&mut app, Keysym::Right, false);
    assert_eq!(app.lib.button, Some(Button::Updates), "nothing right of it");
    key(&mut app, Keysym::Left, false);
    assert_eq!(app.lib.button, Some(Button::Add));
    key(&mut app, Keysym::Down, false);
    assert_eq!(tile(&app), (None, 0));
    // Tab walks the rows, then the head, then the first row again; Shift+Tab the other way.
    for r in 1..rows {
        key(&mut app, Keysym::Tab, false);
        assert_eq!(tile(&app), (None, r));
    }
    key(&mut app, Keysym::Tab, false);
    assert_eq!(app.lib.button, Some(Button::Add));
    key(&mut app, Keysym::Tab, false);
    assert_eq!(app.lib.button, Some(Button::Updates));
    key(&mut app, Keysym::Tab, false);
    assert_eq!(tile(&app), (None, 0));
    key(&mut app, Keysym::Tab, true);
    assert_eq!(app.lib.button, Some(Button::Updates));
    key(&mut app, Keysym::ISO_Left_Tab, false);
    assert_eq!(app.lib.button, Some(Button::Add));
    key(&mut app, Keysym::Tab, true);
    assert_eq!(tile(&app), (None, rows - 1));
    let hot = app.library_hot();
    assert!(hot.contains("library add at=") && hot.contains("library updates at=") && hot.contains("count=0"), "{hot}");
    assert!(!hot.contains("add-search"), "{hot}");
    assert!(app.library_state().lines().next().unwrap().ends_with(" button=-"));
}

#[test]
fn a_search_without_hits_focuses_its_link_and_hands_the_text_on() {
    let (_d, mut app) = new_app();
    app.library_open("test");
    for c in "zzq".chars() {
        app.library_key(Keysym::from_char(c), &ModifiersState::default());
    }
    assert!(app.library_empty() && app.lib.note.is_none());
    assert_eq!(app.lib.button, Some(Button::AddSearch), "Enter opens the link");
    assert_eq!(Button::AddSearch.address(&app.lib.query), "programme/hinzufuegen/zzq");
    let hot = app.library_hot();
    assert!(hot.contains("library add-search at=") && hot.contains("query=\"zzq\" focus=true"), "{hot}");
    assert!(app.library_state().lines().next().unwrap().ends_with(" button=add-search"));
    // Its link is where the pointer finds it; Up reaches the head, Tab comes back through it to the link.
    let (_, link, _) = view::empty(&app.library_geo(), &app.theme);
    assert_eq!(app.library_button_at(link.x as f64 + 2.0, link.y as f64 + 2.0), Some(Button::AddSearch));
    key(&mut app, Keysym::Up, false);
    assert_eq!(app.lib.button, Some(Button::Add));
    key(&mut app, Keysym::Tab, false);
    key(&mut app, Keysym::Tab, false);
    assert_eq!(app.lib.button, Some(Button::AddSearch));
    // Backspace until something matches again: the tiles take the focus back.
    for _ in 0..3 {
        key(&mut app, Keysym::BackSpace, false);
    }
    assert_eq!(app.lib.button, None);
    assert!(!app.library_hot().contains("add-search"));
    app.library_close("test");
    assert!(app.library_hot().is_empty(), "nothing while closed");
}

#[test]
fn the_count_is_the_pages_last_check_and_the_overlay_asks_nothing() {
    let (_d, mut app) = new_app();
    app.library_open("test");
    assert_eq!(app.library_updates_count(), None);
    assert!(app.prog.pending.is_empty(), "opening the overlay sends no request");
    app.prog.updates.push(crate::programme::Update { name: "jq".into(), ..Default::default() });
    app.prog.updates.push(crate::programme::Update { name: "ripgrep".into(), ..Default::default() });
    assert_eq!(app.library_updates_count(), Some(2));
    assert!(app.library_hot().contains("count=2"));
    let h = app.library_head();
    let u = h.updates.expect("shown");
    assert!(h.add.x + h.add.w < u.x && u.x + u.w < h.esc.x && h.text.x + h.text.w < h.add.x, "{h:?}");
    assert_eq!(app.library_button_at(u.x as f64 + 2.0, u.y as f64 + 2.0), Some(Button::Updates));
    assert!(app.prog.pending.is_empty());
}
