//! EINS-B: the popup as a view of the one assistant, against the scripted fake of K1 (`assist_fake`).

use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::assist_fake::{self, Fake};

fn app() -> (Display<App>, App, Fake) {
    assist_fake::app()
}

/// The popup open over the desktop, its chat made, its picture taken.
fn opened(a: &mut App, f: &mut Fake) -> String {
    a.input_origin = Origin::Physical;
    a.db_open("Win+F1");
    assert!(a.db.open);
    assert_eq!(a.db.stage, Stage::Input, "it opens with the input line");
    assert_eq!(a.db.focus, None, "the input line has the focus");
    f.pump(a);
    let g = a.db.generation;
    a.db_msg(g, Msg::Shot(Some(Shot { w: 2, h: 1, png: b"PNG".to_vec(), thumb: Vec::new(), thumb_w: 0, thumb_h: 0 })));
    a.db.chat.clone().expect("the service made the popup's chat")
}

fn typed(a: &mut App, text: &str) {
    for c in text.chars() {
        assert!(a.db_key(Keysym::from_char(c), &ModifiersState::default()));
    }
}

fn enter(a: &mut App) {
    assert!(a.db_key(Keysym::Return, &ModifiersState::default()));
}

fn asks(a: &mut App, f: &mut Fake) -> Vec<Json> {
    f.got(a, "ask")
}

#[test]
fn each_opening_is_a_new_chat_of_origin_popup() {
    let (_d, mut a, mut f) = app();
    let c1 = opened(&mut a, &mut f);
    a.db_close("test");
    let c2 = opened(&mut a, &mut f);
    assert_ne!(c1, c2, "Win+F1 starts a fresh chat");
    assert_eq!(a.db_sees(), "sieht: bitscomp · Arbeitsfläche", "it shows which window it sees");
}

#[test]
fn a_typed_first_question_carries_the_window_without_a_preview() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    typed(&mut a, "such nach Georg");
    enter(&mut a);
    f.pump(&mut a);
    let q = asks(&mut a, &mut f);
    assert_eq!(q.len(), 1, "one question, no preview, no picture question");
    let q = &q[0];
    assert_eq!(q.get("chat").and_then(Json::as_str), Some(chat.as_str()));
    assert_eq!(q.get("text").and_then(Json::as_str), Some("such nach Georg"));
    assert_eq!(q.get("by").and_then(Json::as_str), Some("hand"));
    let c = q.get("context").expect("the window goes along");
    assert_eq!(c.get("window").and_then(|w| w.get("id")).and_then(Json::as_str), Some(crate::ui_reg::DESKTOP));
    assert!(c.get("list").and_then(Json::as_str).is_some_and(|l| l.contains("\"Programme\"")), "the element list");
    assert_eq!(c.get("image").and_then(|i| i.get("media_type")).and_then(Json::as_str), Some("image/png"), "and the picture");
    assert_eq!(a.db.stage, Stage::Waiting);
    // A follow-up carries no window again (the model reads it with `sehen`).
    f.ev(&mut a, &chat, "done", "");
    typed(&mut a, "und dann?");
    enter(&mut a);
    let q = asks(&mut a, &mut f);
    assert_eq!(q[0].get("text").and_then(Json::as_str), Some("und dann?"));
    assert!(q[0].get("context").is_none(), "{:?}", q[0]);
}

#[test]
fn enter_on_the_empty_line_asks_what_georg_sees() {
    let (_d, mut a, mut f) = app();
    opened(&mut a, &mut f);
    enter(&mut a);
    let q = asks(&mut a, &mut f);
    assert_eq!(q[0].get("text").and_then(Json::as_str), Some(q::DEFAULT));
    assert_eq!(q::DEFAULT, "Erklär mir, was ich hier sehe");
}

#[test]
fn a_question_typed_before_the_chat_is_there_waits_for_it() {
    let (_d, mut a, mut f) = app();
    a.input_origin = Origin::Physical;
    a.db_open("Win+F1");
    // Enter before the service answered `new` (and before the picture): the question waits.
    enter(&mut a);
    assert!(asks(&mut a, &mut f).is_empty());
    let g = a.db.generation;
    a.db_msg(g, Msg::Shot(None));
    f.pump(&mut a);
    let q = asks(&mut a, &mut f);
    assert_eq!(q.len(), 1);
    assert!(q[0].get("context").and_then(|c| c.get("image")).is_none(), "no picture could be made: none goes");
}

#[test]
fn a_never_see_program_sends_nothing_and_the_popup_says_so() {
    let (_d, mut a, mut f) = app();
    a.settings.set(crate::assist_level::NEVER_SEE, "bitscomp");
    a.input_origin = Origin::Physical;
    a.db_open("Win+F1");
    f.pump(&mut a);
    assert!(a.db.hidden && a.db.want_shot.is_none(), "no picture is taken");
    assert_eq!(a.db_sees(), "sieht nicht: bitscomp");
    assert!(a.db_texts().contains(crate::assist_level::NOT_SEEN), "{}", a.db_texts());
    enter(&mut a);
    let q = asks(&mut a, &mut f);
    assert_eq!(q.len(), 1);
    assert!(q[0].get("context").is_none(), "nothing of the window: {:?}", q[0]);
}

#[test]
fn the_answer_frames_what_it_names_and_the_cloud_line_shows() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    enter(&mut a);
    let w = a.db.snapshot.clone().unwrap();
    let e = w.walk().into_iter().find(|e| e.name == "Programme").unwrap().clone();
    f.ev(&mut a, &chat, "delta", &format!(r#""text":"Mit „Programme“ [#{}] öffnest du""#, e.id));
    f.ev(&mut a, &chat, "delta", r#""text":" die Bibliothek.""#);
    assert_eq!(a.db.stage, Stage::Answering);
    f.ev(&mut a, &chat, "done", "");
    assert_eq!((a.db.stage.clone(), a.db.shown.as_str()), (Stage::Done, "Mit „Programme“ öffnest du die Bibliothek."));
    assert_eq!(a.db_frames(), vec![e.rect]);
    assert_eq!(a.assist_cloud_line(false).as_deref(), Some("geht an Claude (Cloud)"), "board 1's yellow line");
    a.settings.set("ai.cloud_notice", "false");
    assert_eq!(a.assist_cloud_line(false), None);
}

#[test]
fn tool_lines_and_the_engines_question_are_asked_here_only_georgs_hand_says_yes() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    enter(&mut a);
    f.ev(&mut a, &chat, "tool", r#""state":"runs","name":"befehl","line":"Befehl bits dateien suchen downloads""#);
    assert_eq!(a.db.tools.len(), 1);
    f.ev(&mut a, &chat, "tool", r#""state":"asks","name":"loeschen","line":"Soll ich die 3 doppelten Dateien löschen?","ask_id":7"#);
    assert_eq!(a.db.stage, Stage::Step);
    assert_eq!(a.db_buttons()[..2], [Button::StepYes, Button::StepNo]);
    // Injected input never says yes; Georg's Enter too soon neither.
    a.input_origin = Origin::Injected;
    a.db_press(Button::StepYes);
    assert_eq!((a.db.stage.clone(), a.db.note.as_deref()), (Stage::Step, Some(REAL_INPUT)));
    a.input_origin = Origin::Physical;
    enter(&mut a);
    assert!(f.got(&mut a, "answer").is_empty(), "a key meant for what was there before");
    a.db.since = Instant::now() - ARM;
    enter(&mut a);
    let ans = f.got(&mut a, "answer");
    assert_eq!((ans[0].get("ask_id").and_then(Json::as_u64), ans[0].get("yes").and_then(Json::as_bool)), (Some(7), Some(true)));
    assert_eq!(a.db.stage, Stage::Waiting);
    f.ev(&mut a, &chat, "tool", r#""state":"done","name":"loeschen","line":"""#);
    assert_eq!(a.db.note.as_deref(), Some(TRASH_NOTE), "board 5: in the trash, and how to look");
    // Esc at a question is Nein.
    f.ev(&mut a, &chat, "tool", r#""state":"asks","name":"befehl","line":"rm -r alt","ask_id":8"#);
    assert!(a.db_key(Keysym::Escape, &ModifiersState::default()));
    let ans = f.got(&mut a, "answer");
    assert_eq!(ans[0].get("yes").and_then(Json::as_bool), Some(false));
    assert!(a.db.open, "Nein does not close");
}

#[test]
fn esc_closes_and_cancels_the_running_question_the_chat_stays() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    enter(&mut a);
    assert!(a.db_key(Keysym::Escape, &ModifiersState::default()));
    assert!(!a.db.open);
    let c = f.got(&mut a, "cancel");
    assert_eq!(c[0].get("chat").and_then(Json::as_str), Some(chat.as_str()));
    assert!(f.got(&mut a, "remove").is_empty(), "the chat is not removed");
}

#[test]
fn the_control_socket_asks_from_outside_and_never_says_yes() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    crate::db_ctl::ctl(&mut a, &["durchblick", "ask"], "durchblick ask öffne die Programme").unwrap().unwrap();
    let q = asks(&mut a, &mut f);
    assert_eq!(q[0].get("by").and_then(Json::as_str), Some("remote"));
    f.ev(&mut a, &chat, "tool", r#""state":"asks","name":"befehl","line":"ls","ask_id":3"#);
    a.input_origin = Origin::Physical;
    a.db.since = Instant::now() - ARM;
    let out = crate::db_ctl::ctl(&mut a, &["durchblick", "press", "ja"], "durchblick press ja").unwrap().unwrap();
    assert!(out.contains("\"stage\":\"step\""), "{out}");
    assert!(f.got(&mut a, "answer").is_empty() && a.db.note.as_deref() == Some(REAL_INPUT));
    assert_eq!(a.input_origin, Origin::Physical, "Georg's origin is his again");
}

#[test]
fn a_question_of_a_chat_no_view_shows_opens_the_popup_on_it() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    enter(&mut a);
    // The popup went without cancelling (as after a restart of the desktop); the service asks in that chat.
    a.db = Db { tx: a.db.tx.clone(), generation: a.db.generation, ..Db::default() };
    f.ev(&mut a, &chat, "tool", r#""state":"asks","name":"loeschen","line":"Soll ich löschen?","ask_id":4"#);
    assert!(a.db.open && a.db.chat.as_deref() == Some(chat.as_str()));
    assert_eq!(a.db.stage, Stage::Step);
}

#[test]
fn two_chats_in_two_views_each_get_their_own_lines() {
    let (_d, mut a, mut f) = app();
    // The sidebar asks in a chat of its own.
    a.assist.edit.text = "Belege für Oktober".into();
    let lines = a.assist.submit(crate::pacer::now(), false);
    a.assist_sync(lines);
    f.pump(&mut a);
    let side = a.assist.chat.clone().expect("the sidebar's chat");
    let popup = opened(&mut a, &mut f);
    assert_ne!(side, popup);
    enter(&mut a);
    let q = asks(&mut a, &mut f);
    assert_eq!(q.iter().filter_map(|j| j.get("chat").and_then(Json::as_str)).collect::<Vec<_>>(), [side.as_str(), popup.as_str()]);
    f.ev(&mut a, &side, "delta", r#""text":"Drei Belege.""#);
    f.ev(&mut a, &popup, "delta", r#""text":"Der Schreibtisch.""#);
    assert_eq!(a.db.shown, "Der Schreibtisch.", "the popup's chat only");
    assert!(a.assist.items.iter().any(|i| i.text() == "Drei Belege.") && !a.assist.items.iter().any(|i| i.text().contains("Schreibtisch")), "the sidebar's only");
    // The popup's chat is in the sidebar's list and can be continued there.
    f.ev(&mut a, &popup, "done", "");
    let row = a.assist.chats.iter().find(|c| c.chat == popup).cloned().expect("in the list");
    assert_eq!(row.origin, crate::assist::ChatOrigin::Popup);
    let lines = a.assist.open_chat(&row.chat, crate::pacer::now());
    a.assist_sync(lines);
    f.pump(&mut a);
    assert!(a.assist.items.iter().any(|i| matches!(i, crate::assist::Item::Tool { name, .. } if name == "handeln")), "its history with the tool lines: {:?}", a.assist.items);
}

#[test]
fn the_level_pill_needs_georgs_own_click_to_loosen() {
    let (_d, mut a, mut f) = app();
    opened(&mut a, &mut f);
    // settingsd not there: more freedom does not hold here either (the gate reads the local value).
    a.db.since = Instant::now() - ARM;
    a.input_origin = Origin::Physical;
    a.db_press(Button::Level);
    a.db_press(Button::LevelPick(Level::Auto));
    assert_eq!(Level::of(&a.settings), Level::Ask, "not loosened without settingsd");
    assert_eq!(a.db.note.as_deref(), Some(crate::assist_views::SETTINGS_DOWN));
    let (w, sent) = crate::settings_write::Writer::for_test();
    a.settings_writer = Some(w);
    a.settings_live = true;
    a.db.since = Instant::now() - ARM;
    a.input_origin = Origin::Physical;
    a.db_press(Button::Level);
    assert!(a.db.level_open && a.db_buttons().contains(&Button::LevelPick(Level::Auto)));
    // Injected input cannot give more freedom.
    a.input_origin = Origin::Injected;
    a.db_press(Button::LevelPick(Level::Auto));
    assert_eq!(Level::of(&a.settings), Level::Ask);
    assert_eq!(a.db.note.as_deref(), Some(crate::assist_views::LOOSEN_REFUSED));
    // Georg's own click does, confirmed to settingsd.
    a.input_origin = Origin::Physical;
    a.db_press(Button::Level);
    a.db_press(Button::LevelPick(Level::Auto));
    assert_eq!(Level::of(&a.settings), Level::Auto);
    let reqs: Vec<_> = sent.try_iter().map(|(_, r, _)| r).collect();
    assert!(reqs.iter().any(|r| matches!(r, crate::settings_write::Req::Set { key, confirmed: true, .. } if key == lv::LEVEL)), "the loosening is confirmed");
    // settingsd refuses it (its leash, or ai.level not registered yet): the gate goes back to the old level.
    a.page_done(crate::settings_write::Done { pid: crate::desk::DESK_PID, ok: false, key: Some(lv::LEVEL.into()), text: "Nicht erlaubt".into(), ask: None });
    assert_eq!(Level::of(&a.settings), Level::Ask, "back to what is stored");
    a.db_press(Button::Level);
    a.db_press(Button::LevelPick(Level::Auto));
    a.page_done(crate::settings_write::Done { pid: crate::desk::DESK_PID, ok: true, key: Some(lv::LEVEL.into()), text: "Geändert".into(), ask: None });
    assert_eq!(Level::of(&a.settings), Level::Auto, "taken: it stays");
    assert!(a.assist_route.loosened.is_empty());
    // Less freedom is anyone's.
    a.input_origin = Origin::Injected;
    a.db_press(Button::Level);
    a.db_press(Button::LevelPick(Level::Answer));
    assert_eq!(Level::of(&a.settings), Level::Answer);
}

#[test]
fn no_assistant_explains_how_to_set_one_up() {
    let (_d, mut a, _f) = app();
    a.assist_client = None;
    a.db_open("test");
    assert!(matches!(&a.db.stage, Stage::Setup(_)));
    assert_eq!(a.db_buttons(), [Button::OpenKi, Button::Close, Button::Level]);
    a.db_key(Keysym::Escape, &ModifiersState::default());
    assert!(!a.db.open, "Esc closes");
    a.db_key(Keysym::F1, &ModifiersState { logo: true, ..Default::default() });
    assert!(a.db.open, "Win+F1 opens");
}

/// INT-EINS1 (gm/62): Georg's keys pick the level from the pill (Tab, Enter, Tab to the choice, Enter); his next
/// Enter on the empty line asks, it does not land on a choice that is gone.
#[test]
fn after_a_level_picked_by_keys_enter_asks() {
    let (_d, mut a, mut f) = app();
    opened(&mut a, &mut f);
    a.db.since = Instant::now() - ARM;
    let tab = |a: &mut App| assert!(a.db_key(Keysym::Tab, &ModifiersState::default()));
    while a.db.focus != Some(Button::Level) {
        tab(&mut a);
    }
    enter(&mut a);
    assert!(a.db.level_open);
    while a.db.focus != Some(Button::LevelPick(Level::Auto)) {
        tab(&mut a);
    }
    enter(&mut a);
    assert_eq!(a.db.focus, None, "the input line has the keys again");
    a.db.focus = Some(Button::LevelPick(Level::Auto));
    enter(&mut a);
    assert_eq!(asks(&mut a, &mut f).len(), 1, "Enter asked (a focus on a choice that is gone counts as the line's)");
}

/// INT-EINS1 (cl/41): a provider set a moment before the service saw it: its old "not ready" shows the set-up
/// stage, the status is asked again, and a ready answer gives the popup its chat.
#[test]
fn a_stale_not_ready_is_asked_again_and_the_popup_gets_its_chat() {
    let (_d, mut a, mut f) = app();
    a.assist_msg(crate::assist_client::Msg::Line(Json::parse(r#"{"ok":true,"name":"Claude","ready":false,"why":"Kein Anbieter.","busy":false,"turns":0}"#).unwrap()));
    a.db_open("Win+F1");
    assert!(matches!(&a.db.stage, Stage::Setup(_)), "{:?}", a.db.stage);
    f.pump(&mut a);
    assert_eq!(a.db.stage, Stage::Input, "the newer status said ready");
    assert!(a.db.chat.is_some(), "the popup asked for its chat and got it");
}

#[test]
fn in_the_sofa_mode_a_question_is_typed_on_the_screens_keyboard() {
    let (_d, mut a, mut f) = app();
    a.sofa_switch(crate::sofa::Way::WinEsc);
    assert!(a.sofa.active);
    opened(&mut a, &mut f);
    assert!(a.db_buttons().contains(&Button::Keyboard), "the keyboard button for pad use");
    // Y opens the on-screen keyboard; the popup steps aside while it types.
    assert!(a.db_pad(crate::pad::Press::Btn(crate::pad::Btn::Y)));
    assert!(a.db_sofa_typing() && a.db_geo().is_none());
    for c in "Und dann?".chars() {
        a.sofa_type(c);
    }
    assert!(a.db_sofa_enter(), "Fertig was the question's");
    assert!(!a.db_sofa_typing());
    let q = asks(&mut a, &mut f);
    assert_eq!(q[0].get("text").and_then(Json::as_str), Some("Und dann?"));
    assert!(a.db_geo().is_some(), "the popup is back");
    assert!(a.db_pad(crate::pad::Press::Btn(crate::pad::Btn::B)));
    assert!(!a.db.open, "B closes");
}

#[test]
fn from_the_gamepad_the_focused_element_counts_not_the_stale_pointer() {
    let (_d, mut a, _f) = app();
    a.db_open("test");
    let w = a.db.snapshot.clone().unwrap();
    let e = w.walk().into_iter().find(|e| e.name == "Programme").unwrap().clone();
    a.db_close("test");
    let s = a.theme.scale;
    a.pointer = ((e.rect[0] + e.rect[2] / 2) as f64 / s, (e.rect[1] + e.rect[3] / 2) as f64 / s).into();
    a.db_open("Win+F1");
    assert_eq!(a.db.pointed.as_deref(), Some(e.id.as_str()), "the keyboard: what the pointer is on");
    a.db_close("test");
    a.db_open(PAD_OPEN);
    assert_ne!(a.db.pointed.as_deref(), Some(e.id.as_str()), "the gamepad: not the parked pointer's element");
}

#[test]
fn a_question_about_what_the_program_did_brings_its_access_log_once() {
    let (_d, mut a, mut f) = app();
    let chat = opened(&mut a, &mut f);
    let g = a.db.generation;
    a.db_msg(g, Msg::Access(Some("Programm bitscomp: 1× gestartet\nliest /data/home/1000/.ssh/id_rsa\n- Verbindung zu example.org Port 443".into())));
    typed(&mut a, "Was hat das Programm im Hintergrund gemacht?");
    enter(&mut a);
    let q = asks(&mut a, &mut f);
    let extras = q[0].get("context").and_then(|c| c.get("extras")).and_then(Json::as_str).unwrap_or("").to_string();
    assert!(extras.contains("example.org") && !extras.contains(".ssh"), "{extras}");
    f.ev(&mut a, &chat, "done", "");
    typed(&mut a, "Und was hat es gelesen?");
    enter(&mut a);
    let q = asks(&mut a, &mut f);
    assert!(q[0].get("context").is_none(), "once per popup");
}

#[test]
fn without_a_connection_the_popup_says_so_and_nothing_waits_for_a_reply() {
    let (_d, mut a, mut f) = app();
    a.assist_msg(crate::assist_client::Msg::Down("gone".into()));
    a.db_open("Win+F1");
    assert!(matches!(&a.db.stage, Stage::Failed(m) if m.contains("antwortet nicht")), "{:?}", a.db.stage);
    a.db_close("test");
    // It comes back: the sidebar's first question gets its own chat, not one meant for the popup before.
    a.assist_msg(crate::assist_client::Msg::Up);
    f.pump(&mut a);
    a.assist.edit.text = "Hallo".into();
    let lines = a.assist.submit(crate::pacer::now(), false);
    a.assist_sync(lines);
    f.pump(&mut a);
    assert!(a.assist.chat.is_some() && asks(&mut a, &mut f).len() == 1, "the sidebar's question went into its chat");
}
