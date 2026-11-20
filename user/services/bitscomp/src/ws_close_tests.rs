use smithay::reexports::wayland_server::Display;

use super::*;
use crate::chrome::Part as C;

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let app = App::new(d.handle(), "wy-ws-close-test".into(), (2560, 1440));
    (d, app)
}

fn names(app: &App) -> Vec<String> {
    app.session_now().spaces.into_iter().map(|s| s.name).collect()
}

#[test]
fn the_question_is_plain_german() {
    let (t, l) = question("Arbeitsraum 2", 3, "Arbeitsraum 1");
    assert_eq!(t, "„Arbeitsraum 2“ schließen?");
    assert_eq!(l[0], "Seine 3 Fenster wandern in „Arbeitsraum 1“.");
    assert_eq!(question("A", 1, "B").1[0], "Sein Fenster wandert in „B“.");
    assert_eq!((neighbour(0), neighbour(1), neighbour(4)), (1, 0, 3));
}

#[test]
fn the_x_is_on_every_entry_with_two_workspaces_and_hits_in_both_presets() {
    let (_d, mut app) = new_app();
    assert!(app.chrome().ws_close.is_empty() && !app.ws_closable(0), "never the only workspace");
    app.ws_new();
    for preset in ["buehne", "classic"] {
        app.settings.set("theme.layout.preset", preset);
        app.apply_settings("theme.layout.preset");
        let c = app.chrome();
        assert_eq!(c.ws_close.len(), 2, "{preset}");
        for (i, x) in c.ws_close.iter().enumerate() {
            let item = c.ws_items[i];
            assert!(x.x >= item.x && x.x + x.w <= item.x + item.w && x.y >= item.y && x.y + x.h <= item.y + item.h, "{preset}: the x lies on its entry");
            assert_eq!(c.hit(x.x as f64 + x.w as f64 / 2.0, x.y as f64 + x.h as f64 / 2.0), Some(C::WsClose(i)), "{preset}");
        }
        let hot = crate::ctl_mouse::hot(&app);
        assert!(hot.lines().filter(|l| l.starts_with("ws ") && l.contains(" close=")).count() == 2, "{preset}: {hot}");
    }
}

#[test]
fn an_empty_workspace_closes_at_once_and_the_session_follows() {
    let (_d, mut app) = new_app();
    app.ws_new();
    app.ws_new();
    assert_eq!(names(&app), ["Arbeitsraum 1", "Arbeitsraum 2", "Arbeitsraum 3"]);
    app.session.due = None;
    app.ws_close_press(1);
    assert!(app.desk.ws_ask.is_none(), "no question for an empty workspace");
    assert_eq!(names(&app), ["Arbeitsraum 1", "Arbeitsraum 3"]);
    assert_eq!(app.desk.active, 1, "the shown workspace stays shown");
    assert!(app.session.due.is_some(), "the session is saved again");
    // The sofa mode's own workspace keeps its x away while the mode is on.
    app.desk.names[1] = crate::sofa::WORKSPACE.into();
    app.sofa.active = true;
    assert!(!app.ws_closable(1) && app.ws_closable(0));
    app.sofa.active = false;
}

#[test]
fn the_question_answers_by_keys_and_never_by_a_stale_workspace() {
    let (_d, mut app) = new_app();
    app.ws_new();
    let ask = |app: &mut App, ws: usize| {
        let name = app.desk.names[ws].clone();
        app.desk.ws_ask = Some(Ask { ws, name, dialog: page_confirm::plain("?".into(), vec!["x".into()], "Schließen"), hover: None, image: None });
    };
    ask(&mut app, 1);
    app.ws_ask_key(Keysym::Return);
    assert!(app.desk.ws_ask.is_none() && app.ws_count() == 2, "Enter starts on Abbrechen");
    ask(&mut app, 1);
    app.ws_ask_key(Keysym::Escape);
    assert_eq!(app.ws_count(), 2);
    ask(&mut app, 1);
    assert!(app.ws_ask_hot().contains("focus=cancel"));
    app.ws_ask_key(Keysym::Tab);
    assert!(app.ws_ask_hot().contains("focus=ok"));
    // Modal: everywhere but its buttons is nothing.
    assert_eq!(app.ws_ask_spot(1.0, 1.0), Some(Spot::WsAsk(None)));
    app.ws_ask_key(Keysym::Return);
    assert_eq!(app.ws_count(), 1, "Schließen closed it");
    assert_eq!(app.desk.active, 0);
    // A question about a workspace that changed meanwhile does nothing.
    app.ws_new();
    ask(&mut app, 1);
    app.desk.names[1] = "Anders".into();
    app.ws_ask_answer(true);
    assert_eq!(app.ws_count(), 2);
}
