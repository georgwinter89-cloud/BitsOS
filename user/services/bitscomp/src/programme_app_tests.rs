//! INT-CL-4: host tests of who may change something on the page "Programme" (D6): only a click that is
//! Georg's in every part, or a key from a real keyboard. The model's own tests are `programme_tests.rs`.

use smithay::input::keyboard::Keysym;
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::input::Origin::{self, Injected, Physical};
use crate::page_app::PageState;
use crate::programme::Card;

const PID: u32 = 7;

fn new_app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "wy-programme-test".into(), (2560, 1440));
    app.pages.by_pid.insert(PID, PageState::new("programme/foot"));
    (d, app)
}

/// A plan that can run, and for a removal its dialog on the page.
fn ready(app: &mut App, action: Action) {
    let mut c = Card::new(action, "foot");
    c.plan_id = Some("p1".into());
    c.phase = Phase::Ready;
    c.asked = action == Action::Remove;
    app.prog.card = Some(c);
    app.prog.note = None;
    let d = (action == Action::Remove).then(|| crate::page_confirm::plain("foot wirklich entfernen?".into(), vec![], "Entfernen"));
    app.pages.by_pid.get_mut(&PID).unwrap().confirm = d;
}

fn hand(app: &mut App, press: Origin, release: Origin, moved: Origin) {
    (app.desk.press_origin, app.input_origin, app.pointer_origin) = (press, release, moved);
}

fn refused(app: &App) -> bool {
    app.prog.note == Some((REAL_INPUT.to_string(), true))
}

/// Every click that is not Georg's in all three parts; the first is what the release alone used to let through.
const MIXED: [(Origin, Origin, Origin); 4] = [(Injected, Physical, Physical), (Physical, Physical, Injected), (Physical, Injected, Physical), (Injected, Injected, Injected)];

#[test]
fn a_plan_runs_only_by_a_click_that_is_georgs_in_every_part() {
    let (_d, mut app) = new_app();
    for (press, release, moved) in MIXED {
        ready(&mut app, Action::Install);
        hand(&mut app, press, release, moved);
        app.page_act(PID, Part::Prog(ProgPart::CardGo));
        assert!(refused(&app), "press={press:?} release={release:?} pointer={moved:?} ran the plan");
        assert_eq!(app.prog.card.as_ref().map(|c| c.phase), Some(Phase::Ready));
    }
    // The other changing buttons ask the same question before anything else.
    for part in [ProgPart::Rollback, ProgPart::Clean, ProgPart::UpdateAll, ProgPart::CheckNow] {
        ready(&mut app, Action::Install);
        hand(&mut app, Injected, Physical, Physical);
        app.page_act(PID, Part::Prog(part));
        assert!(refused(&app), "{part:?}");
    }
    ready(&mut app, Action::Install);
    hand(&mut app, Physical, Physical, Physical);
    app.page_act(PID, Part::Prog(ProgPart::CardGo));
    assert!(!refused(&app) && app.prog.card.as_ref().is_some_and(|c| c.phase != Phase::Ready), "Georg's own click did not start the plan");
}

#[test]
fn a_removal_is_confirmed_only_by_georgs_own_click_or_key() {
    let (_d, mut app) = new_app();
    for (press, release, moved) in MIXED {
        ready(&mut app, Action::Remove);
        hand(&mut app, press, release, moved);
        app.page_act(PID, Part::DialogOk);
        assert!(refused(&app) && app.prog.card.is_none(), "press={press:?} release={release:?} pointer={moved:?} removed the program");
    }
    ready(&mut app, Action::Remove);
    hand(&mut app, Physical, Physical, Physical);
    app.page_act(PID, Part::DialogOk);
    assert!(!refused(&app) && app.prog.card.as_ref().is_some_and(|c| c.phase != Phase::Ready));

    // The keyboard: an injected Tab never puts the pick on "Entfernen", so Georg's Enter cancels.
    ready(&mut app, Action::Remove);
    app.input_origin = Injected;
    app.page_dialog_key(PID, Keysym::Tab);
    assert!(!app.pages.by_pid[&PID].confirm.as_ref().unwrap().on_ok);
    app.input_origin = Physical;
    app.page_dialog_key(PID, Keysym::Return);
    assert!(app.prog.card.is_none() && app.prog.note == Some(("Nicht entfernt (abgebrochen).".to_string(), false)));
    // His own Tab moves it; an injected key then puts it back, an injected Enter on "Entfernen" is refused.
    ready(&mut app, Action::Remove);
    app.page_dialog_key(PID, Keysym::Tab);
    assert!(app.pages.by_pid[&PID].confirm.as_ref().unwrap().on_ok);
    app.input_origin = Injected;
    app.page_dialog_key(PID, Keysym::Down);
    assert!(!app.pages.by_pid[&PID].confirm.as_ref().unwrap().on_ok);
    app.pages.by_pid.get_mut(&PID).unwrap().confirm.as_mut().unwrap().on_ok = true;
    app.page_dialog_key(PID, Keysym::Return);
    assert!(refused(&app) && app.prog.card.is_none());
    // His own Tab and Enter remove, wherever the pointer was last moved from.
    ready(&mut app, Action::Remove);
    hand(&mut app, Injected, Physical, Injected);
    app.page_dialog_key(PID, Keysym::Tab);
    app.page_dialog_key(PID, Keysym::Return);
    assert!(!refused(&app) && app.prog.card.as_ref().is_some_and(|c| c.phase != Phase::Ready));
}
