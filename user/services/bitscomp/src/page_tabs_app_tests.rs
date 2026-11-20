//! WY-SET fixer: host tests of the settings tabs' writes that need the app: "Zurücksetzen" a leash key asks,
//! a consolidated row's yes for more freedom counts only by real input, and an address that names a key
//! finds its tab once the key's row is there.

use std::sync::mpsc::Receiver;
use std::time::Instant;

use smithay::reexports::wayland_server::Display;

use super::*;
use crate::input::Origin;
use crate::page_app::PageState;
use crate::page_schema::{Spec, Ty, builtin, model};
use crate::settings::Msg as Setting;
use crate::settings_write::{Writer, answer};

const PID: u32 = 7;

type Queue = Receiver<(u32, Req, Instant)>;

fn new_app(specs: Vec<Spec>, values: &[(&str, &str)], address: &str) -> (Display<App>, App, Queue) {
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "wy-set-test".into(), (2560, 1440));
    let (w, rx) = Writer::for_test();
    app.settings_writer = Some(w);
    app.settings_live = true;
    app.setting_keys = specs.iter().map(|s| (s.key.clone(), String::new())).collect();
    app.page_model_set(model(&specs));
    for (k, v) in values {
        app.settings_msg(Setting::Changed(k.to_string(), Some(v.to_string())));
    }
    app.pages.by_pid.insert(PID, PageState::new(address));
    app.page_tab_resolve(PID);
    app.page_view_refresh(PID);
    (d, app, rx)
}

fn hand(app: &mut App, origin: Origin) {
    (app.input_origin, app.desk.press_origin, app.pointer_origin) = (origin, origin, origin);
}

fn click(app: &mut App, part: Part, origin: Origin) {
    hand(app, origin);
    app.page_act(PID, part);
}

fn sent(rx: &Queue) -> Vec<Req> {
    rx.try_iter().map(|(_, r, _)| r).collect()
}

fn msg(app: &App) -> Option<(String, bool)> {
    app.pages.by_pid[&PID].msg.clone()
}

fn needs_confirm() -> Json {
    Json::parse(r#"{"ok":false,"error":"needs_confirm","message":"ai.level: Das erlaubt dem Assistenten mehr als bisher, ohne dich zu fragen."}"#).unwrap()
}

fn level_spec() -> Spec {
    let ty = Ty::Choice(vec!["answer".into(), "ask".into(), "auto".into()]);
    Spec { key: "ai.level".into(), ty, default: Some("ask".into()), de: "Wie viel der Assistent darf".into(), names: None, restart: "none".into(), safety: "normal".into() }
}

#[test]
fn resetting_the_level_to_more_freedom_asks_first_and_only_georgs_own_yes_sends_it() {
    // EINS-B: from Nur antworten back to Fragt vorher gives more freedom: the page asks before it sends.
    let (_d, mut app, rx) = new_app(vec![level_spec()], &[("ai.level", "answer")], "einstellungen");
    app.pages.by_pid.get_mut(&PID).unwrap().changed_only = true;
    app.page_view_refresh(PID);
    let r = app.pages.by_pid[&PID].shown.model.rows.iter().position(|x| x.key == "ai.level").expect("listed as changed");
    let sure = Req::Reset { key: "ai.level".into(), confirmed: true };
    for (origin, want) in [(Origin::Injected, vec![]), (Origin::Physical, vec![sure.clone()])] {
        click(&mut app, Part::Reset(r), Origin::Physical);
        assert!(sent(&rx).is_empty(), "nothing before the answer");
        let d = app.pages.by_pid[&PID].confirm.as_ref().expect("the reset asks");
        assert_eq!(d.ask.req, sure);
        click(&mut app, Part::DialogOk, origin);
        assert_eq!(sent(&rx), want, "{origin:?}");
    }
}

#[test]
fn a_reset_settingsd_refuses_for_its_leash_asks_and_only_georgs_own_yes_sends_it() {
    let spec = Spec { key: "context.roots".into(), ty: Ty::Text { max: 4096 }, default: Some(String::new()), de: "Ordner".into(), names: None, restart: "none".into(), safety: "normal".into() };
    let (_d, mut app, rx) = new_app(vec![spec], &[("context.roots", "/data/home/1000/Dokumente")], "einstellungen");
    app.pages.by_pid.get_mut(&PID).unwrap().changed_only = true;
    app.page_view_refresh(PID);
    let r = app.pages.by_pid[&PID].shown.model.rows.iter().position(|x| x.key == "context.roots").expect("listed as changed");
    click(&mut app, Part::Reset(r), Origin::Physical);
    let plain = Req::Reset { key: "context.roots".into(), confirmed: false };
    assert_eq!(sent(&rx), [plain.clone()]);
    // settingsd's leash refuses the plain reset: the page asks, as for a set.
    let sure = Req::Reset { key: "context.roots".into(), confirmed: true };
    for (origin, want) in [(Origin::Injected, vec![]), (Origin::Physical, vec![sure.clone()])] {
        app.page_done(answer(PID, &plain, &needs_confirm()));
        let d = app.pages.by_pid[&PID].confirm.as_ref().expect("the refused reset opens the dialog");
        assert_eq!(d.ask.req, sure);
        click(&mut app, Part::DialogOk, origin);
        assert_eq!(sent(&rx), want, "{origin:?}");
        assert!(app.pages.by_pid[&PID].confirm.is_none());
    }
    app.page_done(answer(PID, &plain, &needs_confirm()));
    click(&mut app, Part::DialogOk, Origin::Injected);
    assert_eq!(msg(&app), Some((crate::ki_ask::REAL_INPUT.into(), true)));
}

#[test]
fn more_freedom_by_the_level_counts_only_by_georgs_own_yes() {
    let (_d, mut app, rx) = new_app(vec![level_spec()], &[("ai.level", "ask")], "einstellungen/assistent");
    let row = app.pages.by_pid[&PID].shown.model.rows.iter().position(|x| x.key == "ai.level").expect("the level's rows");
    let row_kind = app.pages.by_pid[&PID].shown.model.rows[row].kind.clone();
    let Kind::Pills { opts, notes, multi: false, .. } = row_kind else { panic!("three rows with their sentences: {row_kind:?}") };
    assert_eq!(opts.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(), ["Nur antworten", "Fragt vorher", "Automodus"]);
    assert_eq!(notes[2], "Er macht alles ohne Nachfrage: Befehle, Skripte, Dateien, Programme bedienen, Internet.");
    click(&mut app, Part::Pill(row, 2), Origin::Physical);
    assert!(app.page_many_asks(PID) && sent(&rx).is_empty(), "more freedom asks first");
    click(&mut app, Part::DialogOk, Origin::Injected);
    assert!(sent(&rx).is_empty(), "an injected yes loosened the level");
    assert_eq!(msg(&app), Some((crate::ki_ask::REAL_INPUT.into(), true)));
    click(&mut app, Part::Pill(row, 2), Origin::Physical);
    click(&mut app, Part::DialogOk, Origin::Physical);
    assert_eq!(sent(&rx), [Req::Set { key: "ai.level".into(), value: Json::str("auto"), confirmed: true }]);
    // Less freedom goes at once, by anyone.
    app.settings_msg(Setting::Changed("ai.level".into(), Some("auto".into())));
    app.page_view_refresh(PID);
    click(&mut app, Part::Pill(row, 0), Origin::Injected);
    assert!(!app.page_many_asks(PID));
    assert_eq!(sent(&rx), [Req::Set { key: "ai.level".into(), value: Json::str("answer"), confirmed: false }]);
}

#[test]
fn the_never_see_list_takes_a_program_on_and_off() {
    let spec = Spec { key: "assistant.never_see".into(), ty: Ty::Text { max: 4096 }, default: Some(String::new()), de: "Nie".into(), names: None, restart: "none".into(), safety: "normal".into() };
    let (_d, mut app, rx) = new_app(vec![spec], &[("assistant.never_see", "keepassxc")], "einstellungen/assistent");
    let rows = app.pages.by_pid[&PID].shown.model.rows.clone();
    let r = rows.iter().position(|x| x.key == "assistant.never_see").expect("its row");
    assert!(matches!(&rows[r].kind, Kind::Pills { multi: true, opts, .. } if opts[0].0 == "keepassxc"));
    // Taking a program off lets the assistant see it again: asked, and only Georg's own yes counts.
    click(&mut app, Part::Pill(r, 0), Origin::Physical);
    assert!(app.page_many_asks(PID) && sent(&rx).is_empty(), "taking it off asks first");
    let d = app.pages.by_pid[&PID].confirm.clone().expect("the dialog");
    assert!(format!("{d:?}").contains("sieht dann wieder, was dieses Programm zeigt"), "{d:?}");
    click(&mut app, Part::DialogOk, Origin::Injected);
    assert!(sent(&rx).is_empty(), "an injected yes took a program off the list");
    click(&mut app, Part::Pill(r, 0), Origin::Physical);
    click(&mut app, Part::DialogOk, Origin::Physical);
    assert_eq!(sent(&rx), [Req::Set { key: "assistant.never_see".into(), value: Json::str(""), confirmed: true }], "× takes it off");
}

#[test]
fn seeing_the_window_in_front_in_the_sidebar_is_switched_on_only_by_georgs_own_yes() {
    let spec = Spec { key: "assistant.sidebar_sees".into(), ty: Ty::Bool, default: Some("false".into()), de: "Mitsehen".into(), names: None, restart: "none".into(), safety: "normal".into() };
    let (_d, mut app, rx) = new_app(vec![spec], &[], "einstellungen/assistent");
    let rows = app.pages.by_pid[&PID].shown.model.rows.clone();
    let r = rows.iter().position(|x| x.key == "assistant.sidebar_sees").expect("its row");
    click(&mut app, Part::Switch(r), Origin::Physical);
    assert!(app.page_many_asks(PID) && sent(&rx).is_empty(), "switching it on asks first");
    click(&mut app, Part::DialogOk, Origin::Injected);
    assert!(sent(&rx).is_empty(), "an injected yes switched seeing on");
    assert_eq!(msg(&app), Some((crate::ki_ask::REAL_INPUT.into(), true)), "the page says why");
    click(&mut app, Part::Switch(r), Origin::Physical);
    click(&mut app, Part::DialogOk, Origin::Physical);
    assert_eq!(sent(&rx), [Req::Set { key: "assistant.sidebar_sees".into(), value: Json::Bool(true), confirmed: true }]);
    app.settings_msg(Setting::Changed("assistant.sidebar_sees".into(), Some("true".into())));
    app.page_view_refresh(PID);
    click(&mut app, Part::Switch(r), Origin::Injected);
    assert_eq!(sent(&rx), [Req::Set { key: "assistant.sidebar_sees".into(), value: Json::Bool(false), confirmed: false }], "off: at once");
}

#[test]
fn an_address_of_a_key_finds_its_tab_when_the_keys_row_comes_later() {
    let spec = Spec { key: "kernel.snd_output".into(), ty: Ty::Choice(vec!["auto".into(), "hdmi".into()]), default: Some("auto".into()), de: "Tonausgabe".into(), ..builtin()[0].clone() };
    let (_d, mut app, _rx) = new_app(Vec::new(), &[], "einstellungen/kernel/snd_output");
    let s = &app.pages.by_pid[&PID];
    assert!(s.tab == 0 && !s.tab_given && s.jump, "no row yet: the first tab");
    app.pages.by_pid.get_mut(&PID).unwrap().jumped = Some((800, 600));
    app.setting_keys = vec![(spec.key.clone(), String::new())];
    app.page_model_set(model(&[spec.clone()]));
    let ton = page_tabs::tab_of("kernel.snd_output");
    let s = &app.pages.by_pid[&PID];
    assert_eq!(page_tabs::TABS[s.tab].word, page_tabs::TABS[ton].word);
    assert_eq!(s.jumped, None, "the jump is computed again on the right tab");
    assert!(s.shown.model.rows.iter().any(|r| r.key == "kernel.snd_output"));
    // Once Georg moved the page (the jump disarmed), the tab stays where he is.
    let (_d2, mut app, _rx) = new_app(Vec::new(), &[], "einstellungen/kernel/snd_output");
    app.pages.by_pid.get_mut(&PID).unwrap().jump = false;
    app.page_model_set(model(&[spec]));
    assert_eq!(app.pages.by_pid[&PID].tab, 0);
}
