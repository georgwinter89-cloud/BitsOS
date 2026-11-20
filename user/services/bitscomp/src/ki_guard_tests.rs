//! CL-5 fixer: host tests of who has the keyboard while a key is typed, of where the key goes shown in full,
//! of each page's own question and of answers that come after their page closed.

use smithay::input::keyboard::{Keysym, ModifiersState};

use super::tests::{PID, SECRET, answer, arm, click, enter, hand, msg, new_app, open_key, sent, set, typed, with_lokal1, yes};
use super::*;
use crate::input::{InputEvent, Origin};
use crate::ki_guard::{IDLE, IDLE_END, LOST};
use crate::page_app::PageState;
use crate::settings_write::{Ask, LIST_MOVED, Sure};
use crate::text::Weight;

const OTHER: u32 = 8;

fn esc(app: &mut App) {
    app.input_origin = Origin::Physical;
    app.page_key(PID, Keysym::Escape, &ModifiersState::default());
}

#[test]
fn injected_input_reaches_nothing_while_a_key_is_typed() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    // Before a key is typed nothing is kept out.
    assert!(!app.ki_drops(Origin::Injected) && !app.ki_ctl_waits(&["click", "10", "10"]));
    open_key(&mut app);
    typed(&mut app, "sk-ge", Origin::Physical);
    // An injected click into the form's name field (it would show and report the rest of the key) and an
    // injected key are dropped before they do anything.
    assert!(app.ki_drops(Origin::Injected) && !app.ki_drops(Origin::Physical));
    app.handle_input_as(InputEvent::Motion { x: 500.0, y: 500.0 }, Origin::Injected);
    app.handle_input_as(InputEvent::Button { code: 0x110, pressed: true }, Origin::Injected);
    app.handle_input_as(InputEvent::Key { code: 30, pressed: true }, Origin::Injected);
    assert!(app.ki_secret_edit(PID), "injected input ended the key's edit");
    // The control socket only reads meanwhile.
    app.ctl_origin = Origin::Injected;
    for cmd in [&["click", "10", "10"][..], &["key", "30"], &["address", "open", "programm/terminal"], &["address", "x"], &["ws", "2"], &["library"], &["page", "clear"], &["set", "ai.providers", "x"], &["sofa"]] {
        assert!(app.ki_ctl_waits(cmd), "{cmd:?} was let through");
    }
    for cmd in [&["page"][..], &["hot"], &["shot", "/x.ppm"], &["list"], &["theme"]] {
        assert!(!app.ki_ctl_waits(cmd), "{cmd:?} only reads");
    }
    // Georg types on: the rest lands in the key's field, in no other and in no report.
    app.input_origin = Origin::Physical;
    assert!(!app.ki_key_swallowed());
    typed(&mut app, "heim-0815", Origin::Physical);
    assert!(app.ki.form.name.is_empty() && app.ki.form.path.is_empty());
    let rep = app.page_report();
    assert!(rep.contains("edit=field=ai.provider.lokal1.key:hidden") && rep.contains("form=\"\",") && !rep.contains("heim"), "{rep}");
    enter(&mut app, Origin::Physical);
    assert_eq!(sent(&rx).len(), 1);
    // Stored (or ended): injected input and the commands work again.
    assert!(app.ki.typing.is_none() && !app.ki_drops(Origin::Injected) && !app.ki_ctl_waits(&["click", "10", "10"]));
    app.input_origin = Origin::Physical;
    assert!(!app.ki_key_swallowed());
}

#[test]
fn a_key_field_that_loses_the_keyboard_swallows_georgs_keys_until_his_click() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    // Another window took the keyboard (a program that started, the page's window ended).
    open_key(&mut app);
    typed(&mut app, "sk-ge", Origin::Physical);
    app.ki.keys_at = Some(OTHER);
    app.input_origin = Origin::Physical;
    assert!(app.ki_key_swallowed(), "the next key went to whoever took the keyboard");
    assert_eq!(msg(&app), (LOST.into(), true));
    assert!(app.pages.by_pid[&PID].edit.is_none() && app.ki.typing.is_none());
    assert!(app.ki_key_swallowed() && app.ki_key_swallowed(), "the keys after it too");
    // Injected keys are not his: they go their usual way, and injected input is no longer kept out.
    app.input_origin = Origin::Injected;
    assert!(!app.ki_key_swallowed() && !app.ki_drops(Origin::Injected));
    // His next click: the keys are his again.
    app.ki_own_click();
    app.input_origin = Origin::Physical;
    assert!(!app.ki_key_swallowed());
    app.ki.keys_at = Some(PID);
    // The key's row went away under the field (the provider was removed by another program).
    open_key(&mut app);
    typed(&mut app, SECRET, Origin::Physical);
    set(&mut app, &[("ai.providers", "")]);
    assert!(app.pages.by_pid[&PID].edit.is_none());
    app.input_origin = Origin::Physical;
    assert!(app.ki_key_swallowed() && msg(&app) == (LOST.into(), true));
    app.ki_own_click();
    set(&mut app, &[("ai.providers", "lokal1")]);
    // Nothing typed for two minutes: the field ends, so injected input is not kept out for good.
    open_key(&mut app);
    app.ki.typing.as_mut().unwrap().last = crate::pacer::now().saturating_sub(IDLE);
    assert!(!app.ki_drops(Origin::Injected));
    assert_eq!(msg(&app), (IDLE_END.into(), true));
    assert!(app.pages.by_pid[&PID].edit.is_none() && app.ki.swallow);
    app.ki_own_click();
    // What Georg ends himself swallows nothing: Esc, a click elsewhere, a second click into the field.
    open_key(&mut app);
    esc(&mut app);
    assert!(app.ki.typing.is_none() && !app.ki_key_swallowed() && app.pages.by_pid[&PID].edit.is_none());
    open_key(&mut app);
    app.page_edit_end();
    app.ki_own_click();
    assert!(app.ki.typing.is_none() && !app.ki.swallow && !app.ki_key_swallowed());
    open_key(&mut app);
    typed(&mut app, SECRET, Origin::Physical);
    click(&mut app, "field=ai.provider.lokal1.key", Origin::Physical);
    assert!(app.ki.typing.is_none() && app.pages.by_pid[&PID].confirm.is_some() && !app.ki_key_swallowed());
    assert!(sent(&rx).is_empty());
}

#[test]
fn where_the_key_goes_is_shown_whole_or_the_field_stays_closed() {
    use crate::ki_key::{LINES_MAX, host_of, key_lines};
    // The machine is what assist would connect to: nothing before an `@`, no query, no other scheme.
    assert_eq!(host_of("https://api.anthropic.com.lang.evil.example/v1").unwrap(), "api.anthropic.com.lang.evil.example");
    assert_eq!(host_of("http://127.0.0.1:30500/v1").unwrap(), "127.0.0.1");
    assert_eq!(host_of("http://[fd00::1]:8080/v1").unwrap(), "fd00::1");
    assert_eq!(host_of(" https://beispiel.invalid ").unwrap(), "beispiel.invalid");
    for bad in ["https://api.anthropic.com@evil.example/v1", "https://a.example/v1?x=1", "ftp://a.example", "https://", "https://:443/v1", "https://a b/v1", "a.example/v1"] {
        assert!(host_of(bad).is_err(), "{bad}");
    }
    // A 300 character address in a narrow dialog: every character is on the lines, none is cut, the machine
    // has a line of its own.
    let (_d, mut app, _rx) = new_app();
    let font = crate::chrome_draw::font(&app.theme, 15.0, Weight::Medium);
    let width = |s: &str| font.width(s);
    let host = format!("api.anthropic.com.{}.evil.example", "x".repeat(60));
    let address = format!("https://{host}/v1/{}", "pfad-".repeat(60));
    let address = &address[..300];
    let p = ki::Provider { id: "lang".into(), kind: "anthropic".into(), place: "cloud".into(), address: address.into(), ..Default::default() };
    let max = font.width(address) / 5;
    let lines = key_lines(&p, max, width).unwrap();
    assert!(lines.len() <= LINES_MAX && lines.iter().all(|l| width(l) <= max && !l.contains('…')), "{lines:?}");
    let at = |what: &str| lines.iter().position(|l| l == what).unwrap_or_else(|| panic!("no line {what}: {lines:?}"));
    let (machine, whole, kind) = (at("Der Schlüssel geht an diesen Rechner:"), at("Die ganze Adresse:"), at("Art Anthropic, Ort Cloud."));
    assert_eq!(lines[machine + 1..whole].concat(), host);
    assert_eq!(lines[whole + 1..kind].concat(), address);
    // Narrower still, it would need more lines than the dialog draws: no field, and the page says why.
    let no = key_lines(&p, max / 4, width).unwrap_err();
    assert!(no.contains("zu lang, um sie hier ganz zu zeigen") && no.contains("assist --key lang"), "{no}");
    // An address assist would not use takes no key.
    let odd = ki::Provider { address: "https://api.anthropic.com@evil.example/v1".into(), ..p.clone() };
    let why = key_lines(&odd, 4000, width).unwrap_err();
    assert!(why.contains("richtig eintragen, dann den Schlüssel") && why.contains("kein ?, #, @"), "{why}");
    // On the page: the dialog carries the whole address; a change under it asks again with the new one, and
    // the key is bound to what the last dialog showed.
    with_lokal1(&mut app);
    set(&mut app, &[("ai.provider.lokal1.address", address)]);
    click(&mut app, "field=ai.provider.lokal1.key", Origin::Physical);
    let d = app.pages.by_pid[&PID].confirm.clone().expect("the dialog");
    assert!(d.lines.concat().contains(address) && d.lines.concat().contains(&host), "{:?}", d.lines);
    set(&mut app, &[("ai.provider.lokal1.address", "https://anders.invalid/v1")]);
    yes(&mut app, Origin::Physical);
    let d = app.pages.by_pid[&PID].confirm.clone().expect("asked again");
    assert!(app.pages.by_pid[&PID].edit.is_none() && d.lines.iter().any(|l| l == "anders.invalid"), "{:?}", d.lines);
    yes(&mut app, Origin::Physical);
    let t = app.ki.typing.as_ref().expect("the field is open");
    assert!(t.whilst.contains(&("ai.provider.lokal1.address".to_string(), Json::str("https://anders.invalid/v1"))));
}

#[test]
fn each_page_has_its_own_question_and_no_dialog_turns_into_an_undo() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    set(&mut app, &[("ai.providers", "lokal1, zwei")]);
    app.pages.by_pid.insert(OTHER, PageState::new("einstellungen/ki"));
    let remove = |app: &mut App, pid: u32, id: &str| {
        let p = super::tests::part(app, &format!("button=ai.provider.{id}.test=remove"));
        hand(app, Origin::Physical);
        app.page_act(pid, p);
    };
    // Page 7 asks about lokal1, page 8 about zwei; the yes on page 7 still removes lokal1.
    remove(&mut app, PID, "lokal1");
    remove(&mut app, OTHER, "zwei");
    assert!(app.pages.by_pid[&PID].confirm.as_ref().unwrap().title.contains("lokal1") && app.pages.by_pid[&OTHER].confirm.as_ref().unwrap().title.contains("zwei"));
    yes(&mut app, Origin::Physical);
    assert_eq!(sent(&rx), [Req::Guarded { key: "ai.providers".into(), value: Json::str("zwei"), whilst: vec![("ai.providers".into(), Json::str("lokal1, zwei"))] }]);
    // Page 8's question is still its own.
    arm(&mut app, OTHER);
    app.page_act(OTHER, Part::DialogCancel);
    assert_eq!(app.pages.by_pid[&OTHER].msg.clone().unwrap().0, "Anbieter „zwei“ bleibt (abgebrochen).");
    // A dialog of the page whose question is gone sends nothing (its filler request is an undo).
    remove(&mut app, OTHER, "zwei");
    app.ki.asking.remove(&OTHER);
    hand(&mut app, Origin::Physical);
    app.page_act(OTHER, Part::DialogOk);
    assert!(sent(&rx).is_empty() && app.pages.by_pid[&OTHER].confirm.is_none());
    // A page that closes takes its question with it.
    remove(&mut app, OTHER, "zwei");
    app.pages_reap();
    assert!(app.pages.by_pid.is_empty() && app.ki.asking.is_empty());
}

#[test]
fn an_answer_after_the_page_closed_still_ends_the_add_and_the_removal() {
    let (_d, mut app, rx) = new_app();
    click(&mut app, "field=ki.add.name", Origin::Physical);
    typed(&mut app, "neu", Origin::Physical);
    click(&mut app, "choice=ki.add.kind=openai", Origin::Physical);
    click(&mut app, "button=ki.add=add", Origin::Physical);
    assert_eq!(sent(&rx).len(), 1);
    assert!(app.page_report().contains("adding=neu"));
    // The window closes while the first step is on its way: the add goes on to its end without a page.
    let page = app.pages.by_pid.remove(&PID).unwrap();
    answer(&mut app, "ai.providers", true, "Geändert");
    assert_eq!(sent(&rx), [Req::set("ai.provider.neu.kind", Json::str("openai"))]);
    answer(&mut app, "ai.provider.neu.kind", true, "Geändert");
    app.pages.by_pid.insert(PID, page);
    assert!(app.page_report().contains("adding=-"), "{}", app.page_report());
    // A refused step ends it too.
    set(&mut app, &[("ai.providers", "neu")]);
    app.ki.form = ki::Form { name: "zwei".into(), kind: "openai".into(), path: String::new() };
    click(&mut app, "button=ki.add=add", Origin::Physical);
    let page = app.pages.by_pid.remove(&PID).unwrap();
    answer(&mut app, "ai.providers", false, "Abgelehnt");
    app.pages.by_pid.insert(PID, page);
    assert!(sent(&rx).len() == 1 && app.page_report().contains("adding=-"));
    // An add that never got its answer does not block the form for good.
    click(&mut app, "button=ki.add=add", Origin::Physical);
    assert!(sent(&rx).len() == 1 && app.page_report().contains("adding=zwei"));
    click(&mut app, "button=ki.add=add", Origin::Physical);
    assert!(sent(&rx).is_empty() && msg(&app).0.contains("wird gerade angelegt"));
    app.ki.adding.as_mut().unwrap().since = crate::pacer::now().saturating_sub(ADD_STALE);
    click(&mut app, "button=ki.add=add", Origin::Physical);
    assert_eq!(sent(&rx).len(), 1);
    answer(&mut app, "ai.providers", false, "Abgelehnt");
    // A removal whose page closed: its answer is not taken for a later change of the list by another page.
    click(&mut app, "button=ai.provider.neu.test=remove", Origin::Physical);
    yes(&mut app, Origin::Physical);
    assert!(sent(&rx).len() == 1 && app.ki.removing.is_some());
    let page = app.pages.by_pid.remove(&PID).unwrap();
    answer(&mut app, "ai.providers", false, LIST_MOVED);
    assert!(app.ki.removing.is_none());
    app.pages.by_pid.insert(PID, page);
}

#[test]
fn settingsds_own_question_about_a_provider_or_a_key_needs_georgs_yes_too() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    // The general settings page changed the list; settingsd asked for a confirmation.
    app.pages.by_pid.insert(OTHER, PageState::new("einstellungen"));
    let list = Ask { req: Req::Set { key: "ai.providers".into(), value: Json::str(""), confirmed: true }, key: "ai.providers".into(), to: Json::str("") };
    let undo = Ask { req: Req::Undo { confirmed: Some(Sure { key: "ai.provider.lokal1.key".into(), seq: Some(4) }) }, key: "ai.provider.lokal1.key".into(), to: Json::Null };
    for ask in [list, undo] {
        for (pid, origin) in [(OTHER, Origin::Injected), (PID, Origin::Injected)] {
            app.page_ask(pid, ask.clone());
            hand(&mut app, origin);
            app.page_act(pid, Part::DialogOk);
            assert!(sent(&rx).is_empty(), "an injected yes confirmed {}", ask.key);
            assert_eq!(app.pages.by_pid[&pid].msg, Some((REAL_INPUT.into(), true)));
        }
        // By keys: an injected Tab and Enter confirm nothing, Georg's do.
        app.page_ask(OTHER, ask.clone());
        hand(&mut app, Origin::Injected);
        app.page_dialog_key(OTHER, Keysym::Tab);
        app.page_dialog_key(OTHER, Keysym::Return);
        assert!(sent(&rx).is_empty());
        app.page_ask(OTHER, ask.clone());
        hand(&mut app, Origin::Physical);
        app.page_dialog_key(OTHER, Keysym::Tab);
        app.page_dialog_key(OTHER, Keysym::Return);
        assert_eq!(sent(&rx), [ask.req.clone()]);
    }
    // Another key's confirmation is as it was (WY-10): the click on "Ändern" counts whoever made it.
    let other = Ask { req: Req::Set { key: "test.risky".into(), value: Json::Num(2.0), confirmed: true }, key: "test.risky".into(), to: Json::Num(2.0) };
    app.page_ask(OTHER, other.clone());
    hand(&mut app, Origin::Injected);
    app.page_act(OTHER, Part::DialogOk);
    assert_eq!(sent(&rx), [other.req]);
}
