//! REWIND 1: Win+Tab and Shift+Win+Tab step through the workspaces.

use smithay::reexports::wayland_server::Display;

use crate::input::InputEvent;
use crate::input::Origin::Physical;
use crate::state::App;

const KEY_TAB: u32 = 15;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTMETA: u32 = 125;

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let app = App::new(d.handle(), "wy-ws-cycle-test".into(), (2560, 1440));
    (d, app)
}

fn win_tab(app: &mut App, shift: bool) {
    let mut keys = vec![(KEY_LEFTMETA, true)];
    if shift {
        keys.push((KEY_LEFTSHIFT, true));
    }
    keys.extend([(KEY_TAB, true), (KEY_TAB, false)]);
    if shift {
        keys.push((KEY_LEFTSHIFT, false));
    }
    keys.push((KEY_LEFTMETA, false));
    for (code, pressed) in keys {
        app.handle_input_as(InputEvent::Key { code, pressed }, Physical);
    }
}

#[test]
fn win_tab_steps_forward_and_back_and_wraps() {
    let (_d, mut app) = new_app();
    win_tab(&mut app, false);
    assert_eq!(app.desk.active, 0, "one workspace: nothing to switch to");
    app.ws_new();
    app.ws_new();
    app.ws_switch(0);
    let mut seen = Vec::new();
    for _ in 0..4 {
        win_tab(&mut app, false);
        seen.push(app.desk.active);
    }
    assert_eq!(seen, [1, 2, 0, 1]);
    seen.clear();
    for _ in 0..3 {
        win_tab(&mut app, true);
        seen.push(app.desk.active);
    }
    assert_eq!(seen, [0, 2, 1]);
    assert!(!app.lib.open, "Win alone was not seen: Tab made it a combination");
}

#[test]
fn the_library_and_the_feld_close_first_and_the_sofa_mode_keeps_its_screen() {
    let (_d, mut app) = new_app();
    app.ws_new();
    app.ws_switch(0);
    app.library_open("test");
    win_tab(&mut app, false);
    assert!(!app.lib.open && app.desk.active == 1, "the library closed, then the switch");
    app.feld_open("test");
    win_tab(&mut app, true);
    assert!(!app.feld.open && app.desk.active == 0, "the Feld closed, then the switch");
    app.sofa_switch(crate::sofa::Way::CtlOn);
    let before = app.desk.active;
    win_tab(&mut app, false);
    assert!(app.sofa.active && app.desk.active == before, "no switch in the sofa mode");
}
