//! CL-3b: the question card through bitscomp's real input path (`handle_input_as` with a device's origin, as
//! the evdev back end calls it): which clicks and keys reach the assistant as a yes.

use std::sync::mpsc::Receiver;

use mcpreg::Json;
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::assist::ARM;
use crate::input::InputEvent;
use crate::input::Origin::{Injected, Physical};
use crate::pointer::BTN_LEFT;

const KEY_TAB: u32 = 15;
const KEY_ENTER: u32 = 28;

/// A bitscomp without a back end whose assistant is ready and whose tool asks question `ask_id`.
fn with_card(ask_id: u64) -> (Display<App>, App, Receiver<String>) {
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "wy-assist-test".into(), (2000, 1125));
    let (client, lines) = crate::assist_client::Client::detached();
    app.assist_client = Some(client);
    app.assist_msg(Msg::Up);
    for l in [r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":0}"#, r#"{"ok":true,"chats":[]}"#] {
        app.assist_msg(Msg::Line(Json::parse(l).unwrap()));
    }
    app.assist.chat = Some("c1".into());
    let asked = app.assist.ask("Lösch die alte Liste.", crate::pacer::now(), false);
    app.assist_sync(asked);
    app.assist_msg(Msg::Line(Json::parse(&format!(r#"{{"ev":"tool","chat":"c1","id":1,"name":"delete_file","state":"asks","text":"Löscht a.txt","ask_id":{ask_id}}}"#)).unwrap()));
    assert!(app.assist_place().is_some(), "the conversation has a place on this screen");
    while lines.try_recv().is_ok() {}
    (d, app, lines)
}

/// The middle of a part of the card, in the output's pixels.
fn middle(app: &App, yes: bool) -> (f64, f64) {
    let l = app.assist_parts().unwrap();
    let Some(CardKind::Question { yes: y, no: n, .. }) = l.card.map(|c| c.kind) else { panic!("no question card") };
    let (r, s) = (if yes { y } else { n }, app.theme.scale);
    ((r.x + r.w / 2) as f64 * s, (r.y + r.h / 2) as f64 * s)
}

/// Ja counts from now on (the card is as old as `ARM`).
fn arm(app: &mut App) {
    let c = app.assist.card.as_mut().unwrap();
    c.since = crate::pacer::now().saturating_sub(ARM);
}

fn mv(app: &mut App, to: (f64, f64), o: crate::input::Origin) {
    app.handle_input_as(InputEvent::Motion { x: to.0, y: to.1 }, o);
}

fn button(app: &mut App, pressed: bool, o: crate::input::Origin) {
    app.handle_input_as(InputEvent::Button { code: BTN_LEFT, pressed }, o);
}

fn key(app: &mut App, code: u32, o: crate::input::Origin) {
    app.handle_input_as(InputEvent::Key { code, pressed: true }, o);
    app.handle_input_as(InputEvent::Key { code, pressed: false }, o);
}

/// The answers that went to the assistant (a real click into the sidebar also asks its status again).
fn sent(lines: &Receiver<String>) -> Vec<String> {
    lines.try_iter().filter(|l| l.contains(r#""op":"answer""#)).collect()
}

#[test]
fn a_click_of_an_injected_device_on_ja_is_refused_and_a_real_one_answers() {
    let (_d, mut app, lines) = with_card(41);
    let ja = middle(&app, true);
    arm(&mut app);
    mv(&mut app, ja, Injected);
    button(&mut app, true, Injected);
    button(&mut app, false, Injected);
    assert_eq!(sent(&lines), Vec::<String>::new());
    assert_eq!(app.assist.card.as_ref().and_then(|c| c.note.as_deref()), Some("Ja geht nur mit deiner echten Maus oder Tastatur."));
    assert!(!app.assist.focus, "an injected click gives the input line no keys while a question is open");
    arm(&mut app);
    mv(&mut app, ja, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    assert_eq!(sent(&lines), [r#"{"op":"answer","ask_id":41,"yes":true}"#]);
}

#[test]
fn georgs_release_or_click_is_no_yes_when_injected_input_pressed_or_aimed_for_him() {
    // The press is injected, Georg only lets go of his button.
    let (_d, mut app, lines) = with_card(41);
    let ja = middle(&app, true);
    arm(&mut app);
    mv(&mut app, ja, Physical);
    button(&mut app, true, Injected);
    button(&mut app, false, Physical);
    assert_eq!(sent(&lines), Vec::<String>::new());
    // The pointer is put on Ja for him, his whole click follows without a move of his own.
    arm(&mut app);
    mv(&mut app, ja, Injected);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    assert_eq!(sent(&lines), Vec::<String>::new());
    // A real click in the first second after the card came (or was touched by injected input) does nothing.
    mv(&mut app, ja, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    assert_eq!(sent(&lines), Vec::<String>::new());
    assert!(app.assist.card.is_some());
    // Nein by injected input counts.
    let nein = middle(&app, false);
    mv(&mut app, nein, Injected);
    button(&mut app, true, Injected);
    button(&mut app, false, Injected);
    assert_eq!(sent(&lines), [r#"{"op":"answer","ask_id":41,"yes":false}"#]);
}

#[test]
fn injected_keys_cannot_prepare_a_yes_for_georgs_enter() {
    let (_d, mut app, lines) = with_card(43);
    let input = {
        let (r, s) = (app.assist_parts().unwrap().input.rect, app.theme.scale);
        ((r.x + r.w / 2) as f64 * s, (r.y + r.h / 2) as f64 * s)
    };
    // The checker's way: an injected click for the focus, an injected Tab, then Georg's Enter.
    mv(&mut app, input, Injected);
    button(&mut app, true, Injected);
    button(&mut app, false, Injected);
    assert!(!app.assist_takes_keys(), "no focus from injected input while the card is open");
    // Georg gives the focus himself; the injected Tab still moves nothing.
    mv(&mut app, input, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    assert!(app.assist_takes_keys() && app.assist.focus_physical);
    arm(&mut app);
    key(&mut app, KEY_TAB, Injected);
    assert_eq!(app.assist.card.as_ref().map(|c| c.yes_picked), Some(false));
    // His own Tab and Enter (once Ja is armed again after the injected key) say yes.
    arm(&mut app);
    key(&mut app, KEY_TAB, Physical);
    assert_eq!(app.assist.card.as_ref().map(|c| c.yes_picked), Some(true));
    // An injected Enter in between is not his: the pick falls back to Nein and that is what it answers.
    key(&mut app, KEY_ENTER, Injected);
    assert_eq!(sent(&lines), [r#"{"op":"answer","ask_id":43,"yes":false}"#]);
}

#[test]
fn georgs_own_enter_says_yes_and_his_tab_picks_nein() {
    // Boards 2 and 5: Enter = Ja, Esc = Nein; a real Tab moves the pick.
    let (_d, mut app, lines) = with_card(44);
    let input = {
        let (r, s) = (app.assist_parts().unwrap().input.rect, app.theme.scale);
        ((r.x + r.w / 2) as f64 * s, (r.y + r.h / 2) as f64 * s)
    };
    mv(&mut app, input, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    arm(&mut app);
    key(&mut app, KEY_ENTER, Physical);
    assert_eq!(sent(&lines), [r#"{"op":"answer","ask_id":44,"yes":true}"#]);
    let (_d, mut app, lines) = with_card(45);
    mv(&mut app, input, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    arm(&mut app);
    key(&mut app, KEY_TAB, Physical);
    key(&mut app, KEY_ENTER, Physical);
    assert_eq!(sent(&lines), [r#"{"op":"answer","ask_id":45,"yes":false}"#]);
}

/// EINS-D's question of a `bits` in the assistant's tree on the sidebar's card: an injected click is no yes, Georg's
/// own click is, and the engine hears nothing of it (it is the desktop's question).
#[test]
fn a_question_of_bits_is_answered_on_the_card_by_georgs_own_click() {
    let (_d, mut app, lines) = with_card(41);
    // The tool's question first: the question of bits waits for the card.
    let n = crate::ctl_bits_gate::open(&mut app, 42, Some("close 3".into()), "das Fenster „x“ schließen.".into(), true);
    assert_eq!(app.assist.card.as_ref().map(|c| c.of.clone()), Some(crate::assist::CardFor::Tool));
    let ja = middle(&app, true);
    arm(&mut app);
    mv(&mut app, ja, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    assert_eq!(sent(&lines).len(), 1, "the tool's yes went to the engine");
    let c = app.assist.card.clone().expect("the question of bits comes next");
    assert_eq!((c.of, c.text.as_str()), (crate::assist::CardFor::Bits(n), "Der Assistent möchte das Fenster „x“ schließen."));
    let ja = middle(&app, true);
    arm(&mut app);
    mv(&mut app, ja, Injected);
    button(&mut app, true, Injected);
    button(&mut app, false, Injected);
    assert_eq!(crate::ctl_bits_gate::status(n), "wartet", "an injected click is no yes");
    arm(&mut app);
    mv(&mut app, ja, Physical);
    button(&mut app, true, Physical);
    button(&mut app, false, Physical);
    assert_eq!(crate::ctl_bits_gate::status(n), "ja");
    assert!(app.assist.card.is_none() && sent(&lines).is_empty(), "the engine hears nothing of it");
}
