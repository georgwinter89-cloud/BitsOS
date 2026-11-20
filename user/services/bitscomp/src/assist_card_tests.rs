//! CL-3b: who can say yes to a tool's question (R8c): only Georg's own hand, and only once Ja is armed.

use super::card::REAL_INPUT;
use super::*;
use crate::desk::Ring;
use crate::input::Origin::{Injected, Physical};

const S: fn(u64) -> Duration = Duration::from_secs;
const REAL: Hand = Hand { press: Physical, release: Physical, moved: Physical };

fn j(s: &str) -> Json {
    Json::parse(s).unwrap()
}

/// A ready model with a running ask (id 1); `remote`: injected input asked it.
fn asked(remote: bool) -> Model {
    let mut m = Model::new("Claude");
    m.link_up(S(0));
    m.on(&j(r#"{"ok":true,"name":"Mira","provider":"anthropic","place":"cloud","ready":true,"busy":false,"turns":0}"#), S(0));
    m.on(&j(r#"{"ok":true,"chats":[]}"#), S(0));
    m.chat = Some("c1".into());
    assert_eq!(m.ask("Lösch die alte Liste.", S(1), remote).len(), 1);
    m
}

/// The tool's question `ask_id` arrives at second `at`.
fn card(m: &mut Model, ask_id: u64, at: u64) {
    m.on(&j(&format!(r#"{{"ev":"tool","id":1,"name":"delete_file","state":"asks","text":"Löscht die Datei liste-alt.txt","ask_id":{ask_id}}}"#)), S(at));
    assert_eq!((m.phase, m.phase.ring()), (Phase::Waiting, Ring::Waiting));
}

fn yes(id: u64) -> Vec<String> {
    vec![format!(r#"{{"op":"answer","ask_id":{id},"yes":true}}"#)]
}

fn no(id: u64) -> Vec<String> {
    vec![format!(r#"{{"op":"answer","ask_id":{id},"yes":false}}"#)]
}

#[test]
fn a_click_says_yes_only_when_press_release_and_pointer_were_all_real() {
    let mut m = asked(false);
    card(&mut m, 41, 2);
    let c = m.card.clone().unwrap();
    // Boards 2 and 5: Enter = Ja, so the pick starts on Ja (only Georg's own Enter counts, `enter`).
    assert_eq!((c.ask_id, c.text.as_str(), c.yes_picked, c.remote, c.armed), (41, "Löscht die Datei liste-alt.txt", true, false, false));
    // Every way a click is not wholly Georg's: the pointer was put on Ja, the press was made for him, the
    // release was made for him, or all of it.
    let cases = [
        Hand { moved: Injected, ..REAL },
        Hand { press: Injected, ..REAL },
        Hand { release: Injected, ..REAL },
        Hand::all(Injected),
    ];
    for (k, h) in cases.into_iter().enumerate() {
        let at = S(10 + 5 * k as u64);
        assert!(m.click(true, h, at).is_empty(), "{h:?}");
        let c = m.card.as_ref().unwrap();
        assert_eq!((c.note.as_deref(), c.since, c.armed), (Some(REAL_INPUT), at, false), "the question stays, Ja waits anew");
        assert_eq!(m.phase, Phase::Waiting);
    }
    assert_eq!(m.events.iter().filter(|e| e.contains("yes to question 41 from injected input: refused (R8c)")).count(), 4);
    // Georg's own click, once Ja is armed again.
    assert_eq!(m.click(true, REAL, S(25) + ARM), yes(41));
    assert_eq!((m.card.clone(), m.phase), (None, Phase::Thinking));
}

#[test]
fn ja_counts_only_a_second_after_the_card_came_or_moved() {
    let mut m = asked(false);
    card(&mut m, 41, 2);
    // Waiting for Georg has no deadline of the sidebar's own, but the arming is one: the drawing must follow.
    assert_eq!(m.due(), Some(S(2) + ARM));
    // A real click that was on its way to what was there before does nothing: no answer, no reproach.
    assert!(m.click(true, REAL, S(2) + ARM - Duration::from_millis(1)).is_empty());
    assert!(m.card.as_ref().is_some_and(|c| c.note.is_none() && !c.armed));
    m.tick(S(2) + ARM);
    assert!(m.card.as_ref().is_some_and(|c| c.armed));
    assert_eq!(m.due(), None);
    // The buttons moved (the input line grew, a note came): armed anew.
    m.card_moved(S(10));
    assert_eq!((m.card.as_ref().map(|c| c.armed), m.due()), (Some(false), Some(S(10) + ARM)));
    assert!(m.click(true, REAL, S(10)).is_empty());
    assert_eq!(m.click(true, REAL, S(10) + ARM), yes(41));
}

#[test]
fn nein_needs_no_real_hand_and_no_waiting() {
    let mut m = asked(false);
    card(&mut m, 42, 2);
    assert_eq!(m.click(false, Hand::all(Injected), S(2)), no(42));
    m.on(&j(r#"{"ev":"tool","id":1,"name":"delete_file","state":"refused","text":"Nicht gelöscht."}"#), S(2));
    assert!(matches!(m.items.last(), Some(Item::Tool { state: ToolState::Refused, .. })));
    // Injected input's Enter on the pick where it starts (Ja) says nothing and puts the pick on Nein; its next
    // Enter is Nein.
    card(&mut m, 43, 3);
    assert!(m.enter(Injected, S(3)).is_empty());
    assert_eq!(m.card.as_ref().map(|c| c.yes_picked), Some(false));
    assert_eq!(m.enter(Injected, S(4)), no(43));
}

#[test]
fn enter_says_yes_only_with_a_real_tab_a_real_enter_and_a_focus_georg_gave() {
    let mut m = asked(false);
    card(&mut m, 43, 2);
    m.tick(S(2) + ARM);
    m.focus = true;
    // Injected input gave the input line the keys and tries to move the pick: it stays on Nein.
    m.focus_physical = false;
    m.touched(S(5));
    m.pick(Injected);
    assert_eq!(m.card.as_ref().map(|c| c.yes_picked), Some(false));
    assert!(m.events.iter().any(|e| e.contains("tried to move the pick of question 43: ignored (R8c)")));
    // Georg's own Tab moves it, but his Enter in a sidebar he did not choose to type in does not say yes.
    m.pick(Physical);
    assert_eq!(m.card.as_ref().map(|c| c.yes_picked), Some(true));
    assert!(m.enter(Physical, S(20)).is_empty());
    let c = m.card.clone().unwrap();
    assert_eq!((c.note.as_deref(), c.yes_picked, c.since), (Some(REAL_INPUT), false, S(20)), "refused: the pick is on Nein again");
    // He clicks into the sidebar himself; an injected key in between undoes his pick.
    m.focus_physical = true;
    m.pick(Physical);
    m.touched(S(30));
    assert_eq!(m.card.as_ref().map(|c| (c.yes_picked, c.armed)), Some((false, false)));
    // His Tab again, then an injected Enter: refused. His own Enter before Ja is armed: nothing.
    m.pick(Physical);
    assert!(m.enter(Injected, S(40)).is_empty());
    m.pick(Physical);
    assert!(m.enter(Physical, S(40) + ARM - Duration::from_millis(1)).is_empty());
    assert_eq!(m.card.as_ref().map(|c| c.yes_picked), Some(true), "too early is not a refusal");
    assert_eq!(m.enter(Physical, S(40) + ARM), yes(43));
}

#[test]
fn enter_while_georg_types_his_next_question_is_no_answer() {
    let mut m = asked(false);
    card(&mut m, 46, 2);
    m.tick(S(2) + ARM);
    (m.focus, m.focus_physical) = (true, true);
    m.edit.text = "und dann".into();
    assert!(m.enter(Physical, S(5)).is_empty(), "his Enter is meant for his question");
    assert!(m.card.is_some());
    m.edit.text.clear();
    assert_eq!(m.enter(Physical, S(6)), yes(46));
}

#[test]
fn an_answer_after_a_long_think_does_not_end_the_ask() {
    let mut m = asked(false);
    m.on(&j(r#"{"ev":"start","id":1}"#), S(1));
    card(&mut m, 41, 2);
    // Georg takes longer than an answer may pause between two lines.
    let late = S(2) + ASK_IDLE + S(20);
    assert!(m.tick(late).is_empty(), "waiting for Georg has no deadline here");
    assert_eq!(m.click(true, REAL, late), yes(41));
    assert!(m.tick(late + S(1)).is_empty(), "the pause counts from his answer");
    assert_eq!((m.ask_id(), m.phase), (Some(1), Phase::Thinking));
    assert_eq!(m.due(), Some(late + ASK_IDLE));
    assert_eq!(m.tick(late + ASK_IDLE).len(), 1, "and still ends when nothing follows");
}

#[test]
fn a_question_of_an_ask_from_outside_says_so_on_its_card() {
    let mut m = asked(true);
    card(&mut m, 44, 2);
    assert_eq!(m.card.as_ref().map(|c| c.remote), Some(true));
    // The tool ran and asks again: still the ask from outside.
    assert_eq!(m.click(true, REAL, S(2) + ARM), yes(44));
    m.on(&j(r#"{"ev":"tool","id":1,"name":"delete_file","state":"done","text":"Gelöscht."}"#), S(4));
    card(&mut m, 45, 5);
    assert_eq!(m.card.as_ref().map(|c| c.remote), Some(true));
}
