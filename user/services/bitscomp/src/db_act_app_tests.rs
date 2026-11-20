//! EINS-B: the desktop tool `handeln` under the one level (PLAN K2/K3), against the scripted fake of K1.

use smithay::input::keyboard::{Keysym, ModifiersState};

use super::*;
use crate::assist_fake::{self, Fake};
use crate::db_app::{ARM, REAL_INPUT, Stage};
use crate::db_layout::Button;

/// The popup over the desktop, Georg asked in it with his own hand: (fake, chat).
fn asked(a: &mut App, f: &mut Fake, by_hand: bool) -> String {
    a.input_origin = Origin::Physical;
    a.db_open("Win+F1");
    f.pump(a);
    let g = a.db.generation;
    a.db_msg(g, Msg::Shot(None));
    let origin = if by_hand { Origin::Physical } else { Origin::Injected };
    a.db_submit("öffne die Programme", origin);
    let q = f.got(a, "ask");
    assert_eq!(q.len(), 1);
    a.db.chat.clone().unwrap()
}

fn desk_id(a: &App, role: uilist::Role, name: &str) -> String {
    let w = a.ui_windows(&Full::One(crate::ui_reg::DESKTOP.into())).into_iter().next().unwrap();
    w.walk().into_iter().find(|e| e.role == role && e.name == name).unwrap_or_else(|| panic!("no {name}")).id.clone()
}

fn press_programme(a: &App, flag: bool) -> serde_json::Value {
    serde_json::json!({"element": desk_id(a, uilist::Role::Button, "Programme"), "aktion": "press", "name": "Programme", "unumkehrbar": flag, "warum": "Die Bibliothek öffnen"})
}

fn level(a: &mut App, l: Level) {
    a.settings.set(lv::LEVEL, l.word());
}

fn key(a: &mut App, k: Keysym) -> bool {
    a.db_key(k, &ModifiersState::default())
}

#[test]
fn at_fragt_vorher_each_step_is_asked_and_only_georgs_hand_runs_it() {
    let (_d, mut a, mut f) = assist_fake::app();
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Step, "Fragt vorher asks each step");
    assert_eq!(a.db_frames().len(), 1, "the element is framed");
    assert!(a.db_step_text().contains("„Programme“ drücken"), "{}", a.db_step_text());
    assert_eq!(a.db_step_why_asked(), Some(AUTO_WOULD), "board 2: the Automodus would not have asked");
    assert_eq!(crate::db_ctl::state(&a)["note"], AUTO_WOULD, "`ctl durchblick state` names the line as drawn");
    // The socket presses Ja right after Georg's own key: nothing runs.
    a.db.since = Instant::now() - ARM;
    let out = crate::db_ctl::ctl(&mut a, &["durchblick", "press", "ja"], "durchblick press ja").unwrap().unwrap();
    assert!(out.contains("\"stage\":\"step\"") && a.db.note.as_deref() == Some(REAL_INPUT), "{out}");
    assert!(!a.lib.open && f.result(&mut a, "d1").is_none());
    // Georg's own Enter runs it; the model hears what the window shows now.
    a.db.since = Instant::now() - ARM;
    assert!(key(&mut a, Keysym::Return));
    assert!(a.lib.open, "the step pressed Programme");
    let seq = a.act.seq;
    a.db_act_listed(seq);
    let (text, err) = f.result(&mut a, "d1").expect("answered");
    assert!(!err && text.starts_with("Erledigt: „Programme“ drücken.") && text.contains("Suche"), "{text}");
}

#[test]
fn a_yes_too_soon_after_the_step_appeared_counts_not() {
    let (_d, mut a, mut f) = assist_fake::app();
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    a.input_origin = Origin::Physical;
    a.db_press(Button::StepYes);
    assert!(!a.lib.open && a.db.stage == Stage::Step, "a click meant for what was there before");
}

#[test]
fn in_the_automodus_a_normal_step_runs_unasked_with_the_bar_and_esc_stops_it() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Acting, "no question in the Automodus");
    let (bar, _) = a.db_bar().expect("the bar shows while the assistant acts");
    assert!(bar.starts_with(crate::db_act_draw::BAR) && bar.contains("Esc"), "{bar}");
    assert!(a.ui_windows(&Full::One(crate::ui_reg::DESKTOP.into()))[0].walk().iter().any(|e| e.name == crate::db_act_draw::BAR), "the bar is in the list");
    // Esc during the pause: nothing runs, the model is told at once, the bar goes.
    assert!(key(&mut a, Keysym::Escape));
    let (text, err) = f.result(&mut a, "d1").unwrap();
    assert!(err && text == STOPPED, "{text}");
    a.db_act_go(1);
    assert!(!a.lib.open, "the stopped step never ran");
    assert!(a.db_bar().is_none() && a.db.open, "the bar is gone, the popup stays");
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d2", ACT_TOOL, args);
    assert!(f.result(&mut a, "d2").unwrap().1 && !a.lib.open, "the task is over");
}

#[test]
fn after_the_pause_the_automodus_step_runs() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    a.db_act_go(1);
    assert!(a.lib.open);
    let seq = a.act.seq;
    a.db_act_listed(seq);
    assert!(!f.result(&mut a, "d1").unwrap().1);
}

#[test]
fn what_cannot_be_undone_is_asked_at_every_level_and_answer_only_refuses() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, true);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Step, "F2: asked even in the Automodus");
    assert!(a.db_step_text().contains("nicht rückgängig"), "{}", a.db_step_text());
    a.db_press(Button::StepNo);
    assert!(f.result(&mut a, "d1").unwrap().1);
    f.ev(&mut a, &chat, "done", "");
    // Nur antworten: no step at all.
    level(&mut a, Level::Answer);
    a.db_submit("und jetzt?", Origin::Physical);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d2", ACT_TOOL, args);
    let (text, err) = f.result(&mut a, "d2").unwrap();
    assert!(err && text == ANSWER_ONLY && !a.lib.open, "{text}");
}

#[test]
fn a_question_from_outside_asks_every_step_even_in_the_automodus() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, false);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Step);
    assert!(a.act.task.as_ref().is_some_and(|t| !t.by_georg));
    assert!(a.db_bar().is_none(), "no Automodus for a question Georg did not ask himself");
    assert_eq!(a.db_step_why_asked(), Some(crate::assist::REMOTE_CARD));
}

#[test]
fn never_on_the_assistants_own_buttons_a_never_see_program_or_payment_data() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    a.needs_frame = true;
    let close = desk_id(&a, uilist::Role::Button, "Esc schließt");
    f.desk(&mut a, &chat, "d1", ACT_TOOL, serde_json::json!({"element": close, "aktion": "press", "name": "Esc schließt", "unumkehrbar": false, "warum": "zu"}));
    let (text, err) = f.result(&mut a, "d1").unwrap();
    assert!(err && text.contains("nur Georg"), "{text}");
    assert!(a.db.open);
    let addr = desk_id(&a, uilist::Role::Field, "Adresszeile");
    f.desk(&mut a, &chat, "d2", ACT_TOOL, serde_json::json!({"element": addr, "aktion": "type", "text": "4111 1111 1111 1111", "name": "Adresszeile", "unumkehrbar": false, "warum": "x"}));
    let (text, err) = f.result(&mut a, "d2").unwrap();
    assert!(err && text.contains("Zahlungsdaten"), "{text}");
    a.settings.set(lv::NEVER_SEE, "bitscomp");
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d3", ACT_TOOL, args);
    let (text, err) = f.result(&mut a, "d3").unwrap();
    assert!(err && text.contains(lv::NOT_SEEN) && !a.lib.open, "{text}");
}

#[test]
fn one_task_on_the_screen_at_a_time() {
    let (_d, mut a, mut f) = assist_fake::app();
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert!(a.db_acting());
    let args = press_programme(&a, false);
    f.desk(&mut a, "anderer", "d2", ACT_TOOL, args);
    let (text, err) = f.result(&mut a, "d2").unwrap();
    assert!(err && text == OTHER_TASK, "{text}");
}

#[test]
fn typing_reaches_the_window_while_the_popup_has_the_keyboard() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let addr = desk_id(&a, uilist::Role::Field, "Adresszeile");
    f.desk(&mut a, &chat, "d1", ACT_TOOL, serde_json::json!({"element": addr, "aktion": "type", "text": "einstellungen", "name": "Adresszeile", "unumkehrbar": false, "warum": "x"}));
    a.db_act_go(1);
    let seq = a.act.seq;
    a.db_act_listed(seq);
    let (text, err) = f.result(&mut a, "d1").unwrap();
    assert!(!err, "{text}");
    let w = a.ui_windows(&Full::One(crate::ui_reg::DESKTOP.into())).into_iter().next().unwrap();
    let field = w.walk().into_iter().find(|e| e.name == "Adresszeile").unwrap().clone();
    assert_eq!(field.value.as_deref(), Some("einstellungen"), "the keys went to the address bar, not to the popup");
    assert!(a.db.field.text.is_empty());
}

#[test]
fn the_end_of_the_question_ends_the_task() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    f.ev(&mut a, &chat, "done", "");
    assert!(a.act.task.is_none() && a.db_bar().is_none() && a.db.stage == Stage::Done);
}

#[test]
fn esc_while_the_window_is_read_again_still_answers_the_model() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    a.db_act_go(1);
    assert!(a.lib.open && a.act.pending.as_ref().is_some_and(|p| p.phase == Phase::Reading), "the step's input arrived");
    assert!(key(&mut a, Keysym::Escape));
    let (text, err) = f.result(&mut a, "d1").unwrap();
    assert!(!err && text.starts_with("Erledigt: „Programme“ drücken.") && text.contains("angehalten"), "{text}");
    let seq = a.act.seq;
    a.db_act_listed(seq);
    assert!(f.result(&mut a, "d1").is_none(), "answered once");
}

#[test]
fn the_automodus_switched_off_in_the_pause_asks_the_step() {
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Acting);
    level(&mut a, Level::Ask);
    a.db_act_go(1);
    assert!(!a.lib.open, "not run unasked");
    assert_eq!(a.db.stage, Stage::Step);
    a.input_origin = Origin::Physical;
    a.db.since = Instant::now() - ARM;
    assert!(key(&mut a, Keysym::Return));
    assert!(a.lib.open, "Georg's Enter runs it");
}

#[test]
fn nur_antworten_set_while_a_step_waits_refuses_it_at_once() {
    // Asked at Fragt vorher, then Nur antworten: Georg's own Enter runs nothing.
    let (_d, mut a, mut f) = assist_fake::app();
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Step);
    level(&mut a, Level::Answer);
    a.input_origin = Origin::Physical;
    a.db.since = Instant::now() - ARM;
    key(&mut a, Keysym::Return);
    assert!(!a.lib.open, "nothing changed under Nur antworten");
    let (text, err) = f.result(&mut a, "d1").expect("answered");
    assert!(err && text == ANSWER_ONLY, "{text}");
    assert!(!a.db_acting(), "the task ended");
    // In the Automodus's pause: the step is refused, not turned into a question.
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Acting);
    level(&mut a, Level::Answer);
    a.db_act_go(1);
    assert!(!a.lib.open && a.db.stage != Stage::Step, "neither run nor asked");
    assert_eq!(f.result(&mut a, "d1"), Some((ANSWER_ONLY.to_string(), true)));
    // The pill: the step that waits is refused when Georg (or anyone) tightens to Nur antworten.
    let (_d, mut a, mut f) = assist_fake::app();
    let chat = asked(&mut a, &mut f, true);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    a.assist_level_pick(Level::Answer, Origin::Injected, false);
    assert_eq!(Level::of(&a.settings), Level::Answer, "less freedom is anyone's");
    assert_eq!(f.result(&mut a, "d1"), Some((ANSWER_ONLY.to_string(), true)), "refused at once");
    assert!(!a.db_acting());
}

#[test]
fn a_question_from_outside_runs_a_step_only_on_georgs_own_yes() {
    // PLAN K2: `by:remote` never gets more than "Fragt vorher": the socket's Ja counts nothing, Georg's own does.
    let (_d, mut a, mut f) = assist_fake::app();
    level(&mut a, Level::Auto);
    let chat = asked(&mut a, &mut f, false);
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert_eq!(a.db.stage, Stage::Step);
    a.db.since = Instant::now() - ARM;
    let out = crate::db_ctl::ctl(&mut a, &["durchblick", "press", "ja"], "durchblick press ja").unwrap().unwrap();
    assert!(out.contains("\"stage\":\"step\"") && !a.lib.open, "{out}");
    a.input_origin = Origin::Physical;
    a.db.since = Instant::now() - ARM;
    assert!(key(&mut a, Keysym::Return));
    assert!(a.lib.open, "Georg's own Enter: he read the card that says the question came from outside");
}

#[test]
fn a_step_of_a_sidebar_chat_is_asked_in_the_sidebar_and_esc_stops_without_a_popup() {
    let (_d, mut a, mut f) = assist_fake::app();
    a.assist.edit.text = "öffne die Programme".into();
    let lines = a.assist.submit(crate::pacer::now(), false);
    a.assist_sync(lines);
    f.pump(&mut a);
    let chat = a.assist.chat.clone().unwrap();
    assert!(a.assist_place().is_some(), "the sidebar shows");
    let args = press_programme(&a, false);
    f.desk(&mut a, &chat, "d1", ACT_TOOL, args);
    assert!(!a.db.open, "the sidebar shows the chat: no popup");
    assert!(a.assist.card.as_ref().is_some_and(|c| c.of == crate::assist::CardFor::Step), "the step is the sidebar's card");
    let lines = a.assist.click(false, crate::assist::Hand::all(Origin::Injected), crate::pacer::now());
    a.assist_sync(lines);
    let (text, err) = f.result(&mut a, "d1").unwrap();
    assert!(err && text == STOPPED, "anyone may say Nein: {text}");
}

#[test]
fn the_step_budget_and_the_time_limit_end_a_task_in_a_loop() {
    assert_eq!((crate::db_act::STEP_BUDGET, crate::db_act::TIME_LIMIT.as_secs()), (30, 600), "the numbers in eins-b.md");
}
