//! CL-5: host tests of the page "KI": its rows from the settings, its layout and names, and what its clicks
//! and keys send to settingsd (a writer whose queue the test reads; no socket, no thread).

use std::sync::mpsc::Receiver;
use std::time::Instant;

use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::reexports::wayland_server::Display;

use super::*;
use crate::assist::ARM;
use crate::input::Origin;
use crate::ki_ask::{NOT_YOURS, TYPE_NOW};
use crate::layout::Rect;
use crate::page::{Inputs, Row, layout, part_name};
use crate::page_app::PageState;
use crate::settings::Msg as Setting;
use crate::settings_write::{Writer, logged_line, request_line};

pub(super) const PID: u32 = 7;
const C: Rect = Rect { x: 300, y: 160, w: 1200, h: 900 };
pub(super) const SECRET: &str = "sk-geheim-0815";

pub(super) type Queue = Receiver<(u32, Req, Instant)>;

pub(super) fn new_app() -> (Display<App>, App, Queue) {
    let d = Display::<App>::new().expect("display");
    let mut app = App::new(d.handle(), "wy-ki-test".into(), (2560, 1440));
    let (w, rx) = Writer::for_test();
    app.settings_writer = Some(w);
    app.settings_live = true;
    app.pages.by_pid.insert(PID, PageState::new("einstellungen/ki"));
    // No window in a host test: the page counts as the one that has the keyboard.
    app.ki.keys_at = Some(PID);
    app.ki_refresh();
    (d, app, rx)
}

/// Who presses, releases and last moved the pointer from here on.
pub(super) fn hand(app: &mut App, origin: Origin) {
    (app.input_origin, app.desk.press_origin, app.pointer_origin) = (origin, origin, origin);
}

/// The open question has been on the screen long enough for a yes.
pub(super) fn arm(app: &mut App, pid: u32) {
    let a = app.ki.asking.get_mut(&pid).expect("an open question");
    a.since = a.since.saturating_sub(ARM);
}

/// A click on the dialog's yes button, once it counts.
pub(super) fn yes(app: &mut App, origin: Origin) {
    arm(app, PID);
    hand(app, origin);
    app.page_act(PID, Part::DialogOk);
}

/// Georg clicks into the key's field and says yes to where the key goes.
pub(super) fn open_key(app: &mut App) {
    click(app, "field=ai.provider.lokal1.key", Origin::Physical);
    yes(app, Origin::Physical);
    assert!(app.ki_secret_edit(PID), "the key's field is open: {:?}", msg(app));
}

pub(super) fn set(app: &mut App, pairs: &[(&str, &str)]) {
    for (k, v) in pairs {
        app.settings_msg(Setting::Changed(k.to_string(), Some(v.to_string())));
    }
}

/// A local provider with everything but a key.
pub(super) fn with_lokal1(app: &mut App) {
    set(
        app,
        &[
            ("ai.providers", "lokal1"),
            ("ai.provider.lokal1.kind", "openai_compatible"),
            ("ai.provider.lokal1.place", "local"),
            ("ai.provider.lokal1.address", "http://127.0.0.1:30500/v1"),
            ("ai.provider.lokal1.model", "klein"),
        ],
    );
}

pub(super) fn sent(rx: &Queue) -> Vec<Req> {
    rx.try_iter().map(|(_, r, _)| r).collect()
}

pub(super) fn part(app: &App, name: &str) -> Part {
    let rows = &app.ki.built.model.rows;
    let l = layout(Rect { h: 20000, ..C }, &app.ki.built.model, &Inputs { favourites: 0, scroll: 0 });
    l.parts.iter().map(|(p, _)| *p).find(|p| part_name(rows, *p) == name).unwrap_or_else(|| panic!("no part {name}"))
}

pub(super) fn click(app: &mut App, name: &str, origin: Origin) {
    let p = part(app, name);
    hand(app, origin);
    app.page_act(PID, p);
}

pub(super) fn typed(app: &mut App, text: &str, origin: Origin) {
    app.input_origin = origin;
    for c in text.chars() {
        app.page_key(PID, Keysym::new(c as u32), &ModifiersState::default());
    }
}

pub(super) fn enter(app: &mut App, origin: Origin) {
    app.input_origin = origin;
    app.page_key(PID, Keysym::Return, &ModifiersState::default());
}

pub(super) fn answer(app: &mut App, key: &str, ok: bool, text: &str) {
    app.page_done(Done { pid: PID, ok, key: Some(key.into()), text: text.into(), ask: None });
}

pub(super) fn msg(app: &App) -> (String, bool) {
    app.pages.by_pid[&PID].msg.clone().unwrap_or_default()
}

fn row<'a>(b: &'a Built, key: &str) -> (&'a Row, &'a str, &'a str) {
    let i = b.row_of(key).unwrap_or_else(|| panic!("no row {key}"));
    (&b.model.rows[i], b.values[i].as_str(), b.subs[i].as_ref().map_or("", |s| s.0.as_str()))
}

#[test]
fn lists_names_and_sentences_follow_settingsds_rules() {
    assert_eq!(ki::ids(" claude, strata;claude  l-1 UNGÜLTIG unset"), ["claude", "strata", "l-1"]);
    assert_eq!(ki::list_with("a, b", "c"), "a, b, c");
    assert_eq!(ki::list_with("a, b", "a"), "a, b");
    assert_eq!(ki::list_without("a, b, c", "b"), "a, c");
    assert_eq!(ki::toggled("a", "b"), "a, b");
    assert_eq!(ki::toggled("a, b", "a"), "b");
    assert_eq!(ki::toggled("", "a"), "a");
    assert_eq!(ki::provider_of("ai.provider.l-1.address"), Some(("l-1", "address")));
    for no in ["ai.providers", "ai.provider.x", "ai.provider..kind", "ai.provider.x.a.b", "theme.mode"] {
        assert_eq!(ki::provider_of(no), None, "{no}");
    }
    let mut f = ki::Facts::default();
    f.providers.push(ki::Provider { id: "strata".into(), kind: ki::UNSET.into(), place: ki::UNSET.into(), ..Default::default() });
    assert!(ki::check_name("claude", &f).is_ok());
    for (bad, why) in [("", "Erst einen Namen"), ("Claude", "nur Kleinbuchstaben"), ("a b", "nur Kleinbuchstaben"), ("unset", "nur Kleinbuchstaben"), ("strata", "gibt es schon")] {
        assert!(ki::check_name(bad, &f).unwrap_err().contains(why), "{bad}");
    }
    // A key is typed only once it is clear where it will go, and the page says where.
    let p = &mut f.providers[0];
    let wide = |s: &str| s.chars().count() as i32;
    assert!(crate::ki_key::key_lines(p, 80, wide).unwrap_err().contains("die Art, der Ort, die Adresse"));
    (p.kind, p.place, p.address) = ("openai".into(), "cloud".into(), "https://beispiel.invalid/v1".into());
    let lines = crate::ki_key::key_lines(p, 80, wide).unwrap();
    assert_eq!(lines[..4], ["Der Schlüssel geht an diesen Rechner:", "beispiel.invalid", "Die ganze Adresse:", "https://beispiel.invalid/v1"]);
    assert_eq!(lines[4], "Art OpenAI, Ort Cloud.");
    p.kind = ki::PROGRAM.into();
    assert!(crate::ki_key::key_lines(p, 80, wide).unwrap_err().contains("braucht keinen Schlüssel"));
    // The removal's dialog says that the stored key goes too, and which jobs lose the provider.
    p.stored = true;
    f.roles.insert("ai.role.chat_fallback".into(), "x, strata".into());
    let lines = ki::remove_lines(&f, "strata").join(" | ");
    assert!(lines.contains("Der gespeicherte Schlüssel wird mit gelöscht."), "{lines}");
    // PLAN K8: one role is left; the job is named once, from its fallback too.
    assert!(lines.contains("Gespräch mit dem Assistenten") && !lines.contains("Hintergrund"), "{lines}");
    f.providers[0].stored = false;
    assert!(ki::remove_lines(&f, "strata").join(" ").contains("kein Schlüssel gespeichert"));
    // A test's line and how it reads.
    assert_eq!(ki::test_line(Some(&Test::Done { code: Some(0), line: "erreichbar".into() })), ("erreichbar".into(), Tone::Good));
    assert_eq!(ki::test_line(Some(&Test::Done { code: Some(1), line: "nicht erreichbar".into() })).1, Tone::Bad);
    assert_eq!(ki::test_line(Some(&Test::Done { code: Some(2), line: "fehlt die Art".into() })).1, Tone::Plain);
    assert!(ki::test_line(Some(&Test::Done { code: None, line: String::new() })).0.contains("nichts gesagt"));
    assert!(ki::test_line(Some(&Test::Late)).0.contains("65 Sekunden"));
    assert!(ki::test_line(None).0.contains("Noch nicht getestet"));
}

#[test]
fn the_page_is_built_from_what_settingsd_said() {
    let (_d, mut app, _rx) = new_app();
    // Nothing set: the name, the switches, the jobs without a choice, the form; no provider section.
    let b = app.ki.built.clone();
    let heads: Vec<&str> = b.model.sections.iter().map(|s| match s { crate::page::Section::Rows(n, _) => n.as_str(), _ => "?" }).collect();
    assert_eq!(heads, ["ASSISTENT", "WER MACHT WAS", crate::ki_helpers::HEAD, "ANBIETER HINZUFÜGEN"]);
    assert_eq!(row(&b, ki::NAME).1, "Claude");
    assert_eq!((row(&b, ki::LOCAL_ONLY).1, row(&b, ki::NOTICE).1), ("false", "true"));
    assert!(row(&b, ki::LOCAL_ONLY).2.contains("Kein Cloud-Anbieter wird mehr gefragt"), "one plain sentence what it does");
    assert!(matches!(&row(&b, "ai.role.chat").0.kind, Kind::Pills { opts, multi: false, .. } if opts.len() == 1 && opts[0].1 == "Keiner"));
    assert!(matches!(&row(&b, "ai.role.chat_fallback").0.kind, Kind::Fixed(_)));
    assert!(b.row_of(ki::ADD_PATH).is_none(), "the helper's path only for a helper");
    app.ki.form.kind = ki::PROGRAM.into();
    app.ki_refresh();
    assert!(app.ki.built.row_of(ki::ADD_PATH).is_some());
    // The form never offers "not set".
    assert!(matches!(&row(&app.ki.built, ki::ADD_KIND).0.kind, Kind::Pills { opts, .. } if opts.iter().map(|o| o.0.as_str()).collect::<Vec<_>>() == ["anthropic", "openai", "gemini", "openai_compatible", "program"]));

    with_lokal1(&mut app);
    set(&mut app, &[("ai.providers", "lokal1, wolke, helfer"), ("ai.provider.wolke.kind", "anthropic"), ("ai.provider.wolke.place", "cloud"), ("ai.provider.helfer.kind", "program")]);
    app.settings_msg(Setting::Stored("ai.provider.wolke.key".into(), true));
    set(&mut app, &[("ai.role.chat", "wolke"), ("ai.role.chat_fallback", "lokal1, wolke"), ("ai.role.search", "wolke"), ("ai.role.jobs", "weg")]);
    // PLAN K8: the search and background roles are gone; a value an older settingsd still sends shows nowhere.
    let b = app.ki.built.clone();
    let heads: Vec<&str> = b.model.sections.iter().map(|s| match s { crate::page::Section::Rows(n, _) => n.as_str(), _ => "?" }).collect();
    assert_eq!(heads, ["ASSISTENT", "WER MACHT WAS", crate::ki_helpers::HEAD, "ANBIETER „lokal1“", "ANBIETER „wolke“", "ANBIETER „helfer“", "ANBIETER HINZUFÜGEN"]);
    // Kind and place with German names; the place is never guessed.
    let (r, v, _) = row(&b, "ai.provider.lokal1.kind");
    assert_eq!(v, "openai_compatible");
    assert!(matches!(&r.kind, Kind::Pills { opts, .. } if opts.iter().any(|o| o.1 == "OpenAI-kompatibel") && opts.iter().any(|o| o.1 == "Hilfsprogramm")));
    assert_eq!(row(&b, "ai.provider.helfer.place").1, "unset");
    assert!(matches!(&row(&b, "ai.provider.wolke.place").0.kind, Kind::Pills { opts, .. } if opts.iter().map(|o| o.1.as_str()).collect::<Vec<_>>() == ["Nicht gesetzt", "Cloud", "Lokal"]));
    // The key: only "gespeichert" or "fehlt"; a helper has none.
    assert_eq!((row(&b, "ai.provider.wolke.key").1, &row(&b, "ai.provider.wolke.key").0.kind), ("gespeichert", &Kind::Secret { clear: true }));
    assert_eq!((row(&b, "ai.provider.lokal1.key").1, &row(&b, "ai.provider.lokal1.key").0.kind), ("fehlt", &Kind::Secret { clear: false }));
    assert!(matches!(&row(&b, "ai.provider.helfer.key").0.kind, Kind::Fixed(_)));
    assert_eq!(row(&b, "ai.provider.helfer.address").0.label, "Pfad des Hilfsprogramms");
    // The one job: every provider for the chat; a name that is gone is said to be gone.
    let names = |key: &str| match &row(&b, key).0.kind {
        Kind::Pills { opts, .. } => opts.iter().map(|o| o.0.clone()).collect::<Vec<_>>(),
        k => panic!("{key}: {k:?}"),
    };
    assert_eq!(names("ai.role.chat"), ["", "lokal1", "wolke", "helfer"]);
    for gone in ["ai.role.search", "ai.role.search_fallback", "ai.role.jobs", "ai.role.jobs_fallback"] {
        assert!(b.row_of(gone).is_none(), "{gone}");
    }
    assert!(matches!(&row(&b, "ai.role.chat_fallback").0.kind, Kind::Pills { multi: true, .. }));
    assert_eq!(row(&b, "ai.role.chat_fallback").1, "lokal1,wolke");
    assert!(row(&b, "ai.role.chat_fallback").2.contains("lokal1, dann wolke"));
    set(&mut app, &[("ai.role.chat", "weg")]);
    assert!(row(&app.ki.built, "ai.role.chat").2.contains("„weg“ gibt es nicht"));
    set(&mut app, &[("ai.role.chat", "wolke")]);
    // Nur lokal with a cloud provider for the chat: said at the job.
    set(&mut app, &[("ai.local_only", "true")]);
    assert!(row(&app.ki.built, "ai.role.chat").2.contains("mit „Nur lokal“ wird er nicht gefragt"));
    assert_eq!(row(&app.ki.built, ki::LOCAL_ONLY).1, "true");
    // `ctl page` names the providers and whether each has a key.
    let rep = app.page_report();
    assert!(rep.contains("address=einstellungen/ki") && rep.contains("providers=lokal1,wolke,helfer") && rep.contains("keys=lokal1:fehlt,wolke:gespeichert,helfer:fehlt"), "{rep}");
}

#[test]
fn the_rows_lay_out_without_overlap_and_draw() {
    let (_d, mut app, _rx) = new_app();
    with_lokal1(&mut app);
    set(&mut app, &[("ai.providers", "lokal1, ein-sehr-langer-anbietername-zum-pruefen"), ("ai.role.chat", "lokal1")]);
    app.settings_msg(Setting::Stored("ai.provider.lokal1.key".into(), true));
    app.ki.form.kind = ki::PROGRAM.into();
    app.ki_refresh();
    let m = app.ki.built.model.clone();
    for (w, h) in [(1200, 20000), (560, 20000), (320, 20000)] {
        let c = Rect { w, h, ..C };
        let l = layout(c, &m, &Inputs { favourites: 0, scroll: 0 });
        for (i, (p, a)) in l.parts.iter().enumerate() {
            assert!(a.w > 0 && a.x >= c.x && a.x + a.w <= c.x + c.w, "{w}: {} leaves the page: {a:?}", part_name(&m.rows, *p));
            for (q, b) in &l.parts[i + 1..] {
                let apart = a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(apart, "{w}: {} overlaps {}", part_name(&m.rows, *p), part_name(&m.rows, *q));
            }
        }
        // Every control lies inside its own row.
        let names: Vec<String> = l.parts.iter().map(|(p, _)| part_name(&m.rows, *p)).collect();
        for want in [
            "undo",
            "field=ai.assistant.name",
            "switch=ai.local_only",
            "switch=ai.cloud_notice",
            "choice=ai.role.chat=lokal1",
            "choice=ai.role.chat=",
            "toggle=ai.role.chat_fallback=lokal1",
            "choice=ai.provider.lokal1.kind=program",
            "choice=ai.provider.lokal1.place=cloud",
            "field=ai.provider.lokal1.address",
            "field=ai.provider.lokal1.model",
            "field=ai.provider.lokal1.key",
            "button=ai.provider.lokal1.key=clear",
            "button=ai.provider.lokal1.test=test",
            "button=ai.provider.lokal1.test=remove",
            "field=ki.add.name",
            "choice=ki.add.kind=openai_compatible",
            "field=ki.add.path",
            "button=ki.add=add",
        ] {
            assert!(names.iter().any(|n| n == want), "{w}: {want} in {names:?}");
        }
        // The page draws, in the look of the settings page, with a key's field open.
        app.settings_msg(Setting::Stored("ai.provider.lokal1.key".into(), false));
        open_key(&mut app);
        let look = app.page_look(PID, Rect { h: 900, ..c }).expect("a look");
        assert_eq!(look.title, "KI");
        let canvas = crate::page_draw::draw(&look, crate::theme::test_theme());
        assert!(canvas.w > 0 && canvas.h > 0);
        app.ki_key_own_end();
        app.settings_msg(Setting::Stored("ai.provider.lokal1.key".into(), true));
    }
    // An address with a provider's name jumps to its section.
    let s = PageState::new("einstellungen/ki/lokal1");
    assert_eq!((s.kind, s.prefix.as_str(), s.jump), (PageKind::Ki, "ai.provider.lokal1", true));
    let l = layout(C, &m, &Inputs { favourites: 0, scroll: 0 });
    assert!(l.scroll_to("ai.provider.lokal1", &m.rows, 0).is_some_and(|y| y > 0));
}

#[test]
fn a_provider_is_added_step_by_step_and_only_when_the_form_is_right() {
    let (_d, mut app, rx) = new_app();
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert!(msg(&app).0.contains("Erst einen Namen") && msg(&app).1 && sent(&rx).is_empty());
    click(&mut app, "field=ki.add.name", Origin::Injected);
    typed(&mut app, "Lokal 1", Origin::Injected);
    // A click elsewhere ends the edit; what was typed stays in the form.
    app.page_edit_end();
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert!(msg(&app).0.contains("nur Kleinbuchstaben") && sent(&rx).is_empty(), "{:?}", msg(&app));
    click(&mut app, "field=ki.add.name", Origin::Injected);
    for _ in 0..7 {
        app.page_key(PID, Keysym::BackSpace, &ModifiersState::default());
    }
    typed(&mut app, "lokal1", Origin::Injected);
    enter(&mut app, Origin::Injected);
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert!(msg(&app).0.contains("Erst die Art") && sent(&rx).is_empty());
    click(&mut app, "choice=ki.add.kind=program", Origin::Injected);
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert!(msg(&app).0.contains("Erst den Pfad") && sent(&rx).is_empty());
    click(&mut app, "field=ki.add.path", Origin::Injected);
    typed(&mut app, "/data/hilf", Origin::Injected);
    enter(&mut app, Origin::Injected);
    // The name goes on the list first; the kind and the path only after settingsd took the step before.
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert_eq!(sent(&rx), [Req::set("ai.providers", Json::str("lokal1"))]);
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert!(msg(&app).0.contains("wird gerade angelegt") && sent(&rx).is_empty());
    answer(&mut app, "ai.providers", true, "Geändert");
    assert_eq!(sent(&rx), [Req::set("ai.provider.lokal1.kind", Json::str("program"))]);
    answer(&mut app, "ai.provider.lokal1.kind", true, "Geändert");
    assert_eq!(sent(&rx), [Req::set("ai.provider.lokal1.address", Json::str("/data/hilf"))]);
    answer(&mut app, "ai.provider.lokal1.address", true, "Geändert");
    assert_eq!(msg(&app), ("Anbieter „lokal1“ hinzugefügt. Trag jetzt in seinem Abschnitt den Ort ein.".into(), false));
    assert_eq!(app.ki.form, Form::default());
    assert!(sent(&rx).is_empty() && app.pages.busy == 0);
    // settingsd refuses the name: nothing follows, the form stays.
    app.ki.form = Form { name: "zwei".into(), kind: "openai".into(), path: String::new() };
    app.ki_refresh();
    click(&mut app, "button=ki.add=add", Origin::Injected);
    assert_eq!(sent(&rx).len(), 1);
    answer(&mut app, "ai.providers", false, "Abgelehnt: voll");
    assert_eq!(msg(&app), ("Anbieter „zwei“ nicht hinzugefügt: Abgelehnt: voll".into(), true));
    assert!(sent(&rx).is_empty() && app.ki.form.name == "zwei");
    // Another kind than a helper: two steps.
    click(&mut app, "button=ki.add=add", Origin::Injected);
    answer(&mut app, "ai.providers", true, "Geändert");
    answer(&mut app, "ai.provider.zwei.kind", true, "Geändert");
    assert_eq!(sent(&rx).len(), 2);
    assert!(msg(&app).0.contains("Ort, Adresse und Modell"));
}

#[test]
fn jobs_switches_and_fields_write_their_keys() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    set(&mut app, &[("ai.providers", "lokal1, zwei")]);
    click(&mut app, "choice=ai.role.chat=lokal1", Origin::Injected);
    click(&mut app, "toggle=ai.role.chat_fallback=zwei", Origin::Injected);
    click(&mut app, "switch=ai.local_only", Origin::Injected);
    click(&mut app, "switch=ai.cloud_notice", Origin::Injected);
    click(&mut app, "choice=ai.provider.zwei.place=local", Origin::Injected);
    assert_eq!(
        sent(&rx),
        [
            Req::set("ai.role.chat", Json::str("lokal1")),
            Req::set("ai.role.chat_fallback", Json::str("zwei")),
            Req::set("ai.local_only", Json::Bool(true)),
            Req::set("ai.cloud_notice", Json::Bool(false)),
            Req::set("ai.provider.zwei.place", Json::str("local")),
        ]
    );
    // What settingsd reports is what the page shows: the chosen one is not sent again, a second click on a
    // fallback takes it off, "Keiner" empties the job.
    set(&mut app, &[("ai.role.chat", "lokal1"), ("ai.role.chat_fallback", "zwei, lokal1")]);
    click(&mut app, "choice=ai.role.chat=lokal1", Origin::Injected);
    click(&mut app, "toggle=ai.role.chat_fallback=zwei", Origin::Injected);
    click(&mut app, "choice=ai.role.chat=", Origin::Injected);
    assert_eq!(sent(&rx), [Req::set("ai.role.chat_fallback", Json::str("lokal1")), Req::set("ai.role.chat", Json::str(""))]);
    // A field: Enter writes, Esc does not; the name and a provider's model.
    click(&mut app, "field=ai.assistant.name", Origin::Injected);
    assert_eq!(app.pages.by_pid[&PID].edit.as_ref().map(|e| e.1.text.as_str()), Some("Claude"));
    typed(&mut app, "tte", Origin::Injected);
    enter(&mut app, Origin::Injected);
    click(&mut app, "field=ai.provider.lokal1.model", Origin::Injected);
    typed(&mut app, "er", Origin::Injected);
    app.page_key(PID, Keysym::Escape, &ModifiersState::default());
    assert_eq!(sent(&rx), [Req::set("ai.assistant.name", Json::str("Claudette"))]);
    answer(&mut app, "ai.assistant.name", true, "Geändert");
    assert_eq!(msg(&app), ("Name des Assistenten: Geändert".into(), false));
    // The sidebar's name follows the key (it was `claude.name`).
    set(&mut app, &[("ai.assistant.name", "Claudette")]);
    assert_eq!(app.desk.claude_name, "Claudette");
    // Rückgängig is settingsd's undo.
    app.page_act(PID, Part::Undo);
    assert_eq!(sent(&rx), [Req::UNDO]);
}

#[test]
fn a_key_is_typed_blind_by_real_input_and_never_shown_or_logged() {
    let (_d, mut app, rx) = new_app();
    set(&mut app, &[("ai.providers", "lokal1")]);
    // Not before it is clear where the key will go.
    click(&mut app, "field=ai.provider.lokal1.key", Origin::Physical);
    assert!(msg(&app).0.contains("Erst eintragen: die Art, der Ort, die Adresse") && app.pages.by_pid[&PID].edit.is_none());
    with_lokal1(&mut app);
    // Injected input (MCP, ctl) opens no key field.
    click(&mut app, "field=ai.provider.lokal1.key", Origin::Injected);
    assert_eq!(msg(&app), (REAL_INPUT.into(), true));
    assert!(app.pages.by_pid[&PID].edit.is_none());
    // Georg's click: first the page says in full where the key will go; no field yet.
    click(&mut app, "field=ai.provider.lokal1.key", Origin::Physical);
    let d = app.pages.by_pid[&PID].confirm.clone().expect("the dialog");
    assert_eq!((d.title.as_str(), d.ok.as_str(), d.on_ok), ("Schlüssel für „lokal1“ eintippen?", "Eintippen", false));
    assert_eq!(d.lines[..5], ["Der Schlüssel geht an diesen Rechner:", "127.0.0.1", "Die ganze Adresse:", "http://127.0.0.1:30500/v1", "Art OpenAI-kompatibel, Ort Lokal."]);
    assert!(app.pages.by_pid[&PID].edit.is_none());
    // A yes right after it appeared was not meant for it: the dialog stays.
    hand(&mut app, Origin::Physical);
    app.page_act(PID, Part::DialogOk);
    assert!(app.pages.by_pid[&PID].confirm.is_some() && app.pages.by_pid[&PID].edit.is_none());
    // An injected yes opens no field; Georg's does, and the field is empty.
    yes(&mut app, Origin::Injected);
    assert!(msg(&app) == (REAL_INPUT.into(), true) && app.pages.by_pid[&PID].edit.is_none());
    open_key(&mut app);
    assert_eq!(msg(&app), (TYPE_NOW.into(), false));
    typed(&mut app, SECRET, Origin::Physical);
    // Nothing that can be read from outside holds the key or its length: `ctl page`, the look the image is
    // drawn from (the same for any text), the model.
    let rep = app.page_report();
    assert!(rep.contains("edit=field=ai.provider.lokal1.key:hidden") && !rep.contains("geheim") && !rep.contains(&SECRET.len().to_string()), "{rep}");
    let look = app.page_look(PID, C).unwrap();
    assert!(!format!("{look:?}").contains("geheim"));
    assert_eq!(look.field.as_ref().map(|f| (f.text.as_str(), f.before.as_str())), Some(("", "")));
    app.page_key(PID, Keysym::BackSpace, &ModifiersState::default());
    assert_eq!(app.page_look(PID, C).unwrap(), look, "one character less looks the same");
    typed(&mut app, "5", Origin::Physical);
    assert!(!format!("{:?}", app.ki.built).contains("geheim"));
    // An injected key ends the edit; nothing is stored.
    typed(&mut app, "x", Origin::Injected);
    assert!(app.pages.by_pid[&PID].edit.is_none() && sent(&rx).is_empty());
    assert_eq!(msg(&app), (REAL_INPUT.into(), true));
    // An injected Enter stores nothing either.
    open_key(&mut app);
    typed(&mut app, SECRET, Origin::Physical);
    enter(&mut app, Origin::Injected);
    assert!(app.pages.by_pid[&PID].edit.is_none() && sent(&rx).is_empty());
    // An empty field changes nothing.
    open_key(&mut app);
    enter(&mut app, Origin::Physical);
    assert!(msg(&app).0.contains("Nichts eingetippt") && sent(&rx).is_empty());
    // Georg's Enter: one confirmed change, bound to address, kind and place as the page showed them.
    open_key(&mut app);
    typed(&mut app, &format!("  {SECRET} "), Origin::Physical);
    enter(&mut app, Origin::Physical);
    let reqs = sent(&rx);
    assert_eq!(reqs.len(), 1);
    let line = request_line(&reqs[0]);
    let j = Json::parse(&line).unwrap();
    assert_eq!(j.get("op").and_then(Json::as_str), Some("set"));
    assert_eq!(j.get("key").and_then(Json::as_str), Some("ai.provider.lokal1.key"));
    assert_eq!(j.get("value").and_then(Json::as_str), Some(SECRET));
    assert_eq!(j.get("confirmed").and_then(Json::as_bool), Some(true));
    let w = j.get("while").unwrap();
    assert_eq!(w.get("ai.provider.lokal1.address").and_then(Json::as_str), Some("http://127.0.0.1:30500/v1"));
    assert_eq!(w.get("ai.provider.lokal1.kind").and_then(Json::as_str), Some("openai_compatible"));
    assert_eq!(w.get("ai.provider.lokal1.place").and_then(Json::as_str), Some("local"));
    // The request as a log line or as Debug text never has the key.
    assert!(!logged_line(&reqs[0]).contains("geheim") && logged_line(&reqs[0]).contains("\"value\":\"***\""));
    assert!(!format!("{:?}", reqs[0]).contains("geheim"));
    assert!(!logged_line(&Req::Set { key: "ai.provider.lokal1.key".into(), value: Json::str(SECRET), confirmed: true }).contains("geheim"));
    assert!(!app.page_report().contains("geheim"));
    // settingsd took it; its watch says a value is stored: the row says "gespeichert" and offers "Löschen".
    answer(&mut app, "ai.provider.lokal1.key", true, "Gespeichert");
    assert_eq!(msg(&app), ("Schlüssel für „lokal1“ gespeichert.".into(), false));
    app.settings_msg(Setting::Stored("ai.provider.lokal1.key".into(), true));
    app.settings_msg(Setting::Changed("ai.provider.lokal1.key".into(), None));
    assert_eq!(row(&app.ki.built, "ai.provider.lokal1.key").1, "gespeichert");
    // The address moved while he typed: settingsd's refusal reads as what happened.
    let moved = crate::settings_write::answer(PID, &reqs[0], &Json::parse(r#"{"ok":false,"error":"changed_meanwhile","message":"ai.provider.lokal1.address is not \"x\" (any more): nothing changed"}"#).unwrap());
    assert!(moved.text.starts_with("Nicht gespeichert: Adresse, Art oder Ort haben sich geändert"), "{}", moved.text);
    // A refusal that would repeat the key does not reach the page with it.
    let echo = crate::settings_write::answer(PID, &reqs[0], &Json::parse(&format!(r#"{{"ok":false,"error":"bad_value","message":"{SECRET} passt nicht"}}"#)).unwrap());
    assert!(!echo.text.contains("geheim"), "{}", echo.text);
    // A change of the place now: settingsd's refusal in the page's own words.
    click(&mut app, "choice=ai.provider.lokal1.place=cloud", Origin::Injected);
    assert_eq!(sent(&rx), [Req::set("ai.provider.lokal1.place", Json::str("cloud"))]);
    let bound = "ai.provider.lokal1.place: Für den Anbieter lokal1 ist ein Schlüssel gespeichert; er gilt für Adresse, Art und Ort, wie sie jetzt sind. Erst den Schlüssel löschen (in den Einstellungen auf der Seite „KI“, oder als root an der Konsole: settings reset --confirm ai.provider.lokal1.key), dann das hier ändern, dann den Schlüssel neu eingeben (auf der Seite „KI“ oder mit assist --key lokal1)";
    let d = crate::settings_write::answer(PID, &Req::set("ai.provider.lokal1.place", Json::str("cloud")), &Json::obj(vec![("ok", Json::Bool(false)), ("error", Json::str("key_bound")), ("message", Json::str(bound))]));
    app.page_done(d);
    assert_eq!(msg(&app), (format!("Anbieter „lokal1“, Ort: {}", crate::ki_kinds::key_bound("lokal1")), true));
    // "Löschen" asks first, and only Georg's own yes deletes.
    click(&mut app, "button=ai.provider.lokal1.key=clear", Origin::Injected);
    assert!(app.pages.by_pid[&PID].confirm.as_ref().is_some_and(|d| d.title.contains("Schlüssel von „lokal1“") && !d.on_ok));
    yes(&mut app, Origin::Injected);
    assert!(sent(&rx).is_empty() && msg(&app) == (REAL_INPUT.into(), true));
    // Nor his yes on a question injected input opened.
    click(&mut app, "button=ai.provider.lokal1.key=clear", Origin::Injected);
    yes(&mut app, Origin::Physical);
    assert!(sent(&rx).is_empty() && msg(&app) == (NOT_YOURS.into(), true) && app.pages.by_pid[&PID].confirm.is_none());
    click(&mut app, "button=ai.provider.lokal1.key=clear", Origin::Physical);
    yes(&mut app, Origin::Physical);
    assert_eq!(sent(&rx), [Req::Reset { key: "ai.provider.lokal1.key".into(), confirmed: true }]);
    answer(&mut app, "ai.provider.lokal1.key", true, DONE_RESET);
    assert_eq!(msg(&app).0, "Schlüssel von „lokal1“ gelöscht.");
}

#[test]
fn a_provider_is_removed_only_after_georgs_own_yes() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    set(&mut app, &[("ai.providers", "lokal1, zwei"), ("ai.role.chat", "lokal1")]);
    app.settings_msg(Setting::Stored("ai.provider.lokal1.key".into(), true));
    click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Injected);
    let d = app.pages.by_pid[&PID].confirm.clone().expect("the dialog");
    assert_eq!((d.title.as_str(), d.ok.as_str(), d.on_ok), ("Anbieter „lokal1“ wirklich entfernen?", "Entfernen", false));
    assert!(d.lines.iter().any(|l| l == "Der gespeicherte Schlüssel wird mit gelöscht."), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.contains("Gespräch mit dem Assistenten")), "{:?}", d.lines);
    assert!(app.page_report().contains("dialog=own:\"Anbieter „lokal1“ wirklich entfernen?\":focus=cancel"), "{}", app.page_report());
    // A new list of rows (settingsd's watch) leaves the question open; Abbrechen sends nothing.
    set(&mut app, &[("ai.provider.zwei.model", "m")]);
    app.page_dialogs_drop(Some(&[]), "x");
    assert!(app.pages.by_pid[&PID].confirm.is_some());
    app.page_act(PID, Part::DialogCancel);
    assert!(sent(&rx).is_empty() && msg(&app).0.contains("bleibt (abgebrochen)"));
    // An injected yes removes nothing.
    click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Injected);
    yes(&mut app, Origin::Injected);
    assert!(sent(&rx).is_empty() && msg(&app) == (REAL_INPUT.into(), true) && app.pages.by_pid[&PID].confirm.is_none());
    // Injected input opens the question and moves the pick to "Entfernen"; Georg's Enter, meant for something
    // else, lands on "Abbrechen" again and removes nothing.
    click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Injected);
    app.page_dialog_key(PID, Keysym::Tab);
    assert!(!app.pages.by_pid[&PID].confirm.as_ref().unwrap().on_ok, "an injected Tab moved the pick");
    arm(&mut app, PID);
    hand(&mut app, Origin::Physical);
    app.page_dialog_key(PID, Keysym::Return);
    assert!(sent(&rx).is_empty() && msg(&app).0.contains("bleibt (abgebrochen)"));
    // Even his own Tab and Enter do not count on a question that injected input opened ...
    click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Injected);
    arm(&mut app, PID);
    hand(&mut app, Origin::Physical);
    app.page_dialog_key(PID, Keysym::Tab);
    app.page_dialog_key(PID, Keysym::Return);
    assert!(sent(&rx).is_empty() && msg(&app) == (NOT_YOURS.into(), true) && app.pages.by_pid[&PID].confirm.is_none());
    // ... or touched (and the touch makes the yes wait anew).
    click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Physical);
    arm(&mut app, PID);
    hand(&mut app, Origin::Injected);
    app.page_dialog_key(PID, Keysym::Right);
    hand(&mut app, Origin::Physical);
    app.page_dialog_key(PID, Keysym::Tab);
    app.page_dialog_key(PID, Keysym::Return);
    assert!(sent(&rx).is_empty() && app.pages.by_pid[&PID].confirm.is_some(), "a yes right after the touch counted");
    yes(&mut app, Origin::Physical);
    assert!(sent(&rx).is_empty() && msg(&app) == (NOT_YOURS.into(), true));
    // Injected input puts the pointer on "Entfernen" (or presses for him): his click there is not his own.
    for (press, moved) in [(Origin::Physical, Origin::Injected), (Origin::Injected, Origin::Physical)] {
        click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Physical);
        arm(&mut app, PID);
        (app.input_origin, app.desk.press_origin, app.pointer_origin) = (Origin::Physical, press, moved);
        app.page_act(PID, Part::DialogOk);
        assert!(sent(&rx).is_empty() && msg(&app) == (REAL_INPUT.into(), true) && app.pages.by_pid[&PID].confirm.is_none());
    }
    // Georg's question and Georg's yes, but too soon after it appeared: nothing yet.
    click(&mut app, "button=ai.provider.lokal1.test=remove", Origin::Physical);
    app.page_dialog_key(PID, Keysym::Tab);
    app.page_dialog_key(PID, Keysym::Return);
    assert!(sent(&rx).is_empty() && app.pages.by_pid[&PID].confirm.is_some());
    // His yes: the list without the name, confirmed, and only while the list is the one the dialog was built
    // from (settingsd then drops the values, the key, the jobs).
    arm(&mut app, PID);
    app.page_dialog_key(PID, Keysym::Return);
    let reqs = sent(&rx);
    assert_eq!(reqs, [Req::Guarded { key: "ai.providers".into(), value: Json::str("zwei"), whilst: vec![("ai.providers".into(), Json::str("lokal1, zwei"))] }]);
    let j = Json::parse(&request_line(&reqs[0])).unwrap();
    assert_eq!((j.get("op").and_then(Json::as_str), j.get("confirmed").and_then(Json::as_bool)), (Some("set"), Some(true)));
    assert_eq!(j.get("while").and_then(|w| w.get("ai.providers")).and_then(Json::as_str), Some("lokal1, zwei"));
    answer(&mut app, "ai.providers", true, "Geändert");
    assert_eq!(msg(&app), ("Anbieter „lokal1“ entfernt.".into(), false));
    // The provider went away while the dialog was open: nothing is sent.
    set(&mut app, &[("ai.providers", "zwei")]);
    click(&mut app, "button=ai.provider.zwei.test=remove", Origin::Physical);
    app.settings.set("ai.providers", "");
    yes(&mut app, Origin::Physical);
    assert!(sent(&rx).is_empty() && msg(&app).0.contains("gibt es nicht mehr"));
    // bitscomp's copy of the list is not settingsd's (a provider was added a moment ago, or `ctl set` wrote
    // the copy): the yes carries the list the dialog named, and settingsd's refusal reads as what happened.
    app.settings.set("ai.providers", "zwei");
    click(&mut app, "button=ai.provider.zwei.test=remove", Origin::Physical);
    yes(&mut app, Origin::Physical);
    let reqs = sent(&rx);
    assert_eq!(reqs, [Req::Guarded { key: "ai.providers".into(), value: Json::str(""), whilst: vec![("ai.providers".into(), Json::str("zwei"))] }]);
    let no = crate::settings_write::answer(PID, &reqs[0], &Json::parse(r#"{"ok":false,"error":"changed_meanwhile","message":"ai.providers is not \"zwei\" (any more): nothing changed"}"#).unwrap());
    app.page_done(no);
    assert_eq!(msg(&app), ("Anbieter „zwei“ nicht entfernt: Die Liste der Anbieter hat sich geändert: nichts entfernt, bitte neu ansehen.".into(), true));
}

#[test]
fn a_test_shows_its_one_line_and_an_old_answer_is_dropped() {
    let (_d, mut app, _rx) = new_app();
    with_lokal1(&mut app);
    let sub = |app: &App| app.ki.built.subs[app.ki.built.row_of("ai.provider.lokal1.test").unwrap()].clone().unwrap();
    assert!(sub(&app).0.contains("Noch nicht getestet"));
    // No program to run here (no main loop in this test): the page says so instead of waiting.
    click(&mut app, "button=ai.provider.lokal1.test=test", Origin::Injected);
    assert_eq!(sub(&app), ("Hier lässt sich kein Test starten.".into(), Tone::Bad));
    // A running test, its answer, and the deadline's message after it.
    app.ki.runs.insert("lokal1".into(), 5);
    app.ki.tests.insert("lokal1".into(), Test::Running);
    app.ki_refresh();
    assert!(sub(&app).0.contains("Test läuft"));
    click(&mut app, "button=ai.provider.lokal1.test=test", Origin::Injected);
    assert!(msg(&app).0.contains("läuft schon"));
    app.ki_probe_msg(Msg { id: "lokal1".into(), run: 4, end: End::Late });
    assert_eq!(app.ki.tests["lokal1"], Test::Running, "an older run says nothing");
    let line = "Der Anbieter „lokal1“ ist erreichbar und hat geantwortet.";
    app.ki_probe_msg(Msg { id: "lokal1".into(), run: 5, end: End::Exit(Some(0), line.into()) });
    assert_eq!(sub(&app), (line.into(), Tone::Good));
    assert_eq!(msg(&app), (format!("Test von „lokal1“: {line}"), false));
    app.ki_probe_msg(Msg { id: "lokal1".into(), run: 5, end: End::Late });
    assert_eq!(sub(&app).1, Tone::Good, "the deadline after the answer changes nothing");
    assert!(app.page_report().contains("tests=lokal1:exit=0:"), "{}", app.page_report());
    // The line was about the provider as it was: a change of it takes the line away.
    set(&mut app, &[("ai.provider.lokal1.model", "anders")]);
    assert!(sub(&app).0.contains("Noch nicht getestet"));
    // In vain, and no answer in time.
    app.ki.runs.insert("lokal1".into(), 6);
    app.ki.tests.insert("lokal1".into(), Test::Running);
    app.ki_probe_msg(Msg { id: "lokal1".into(), run: 6, end: End::Exit(Some(1), "Der Anbieter „lokal1“ ist nicht erreichbar.".into()) });
    assert_eq!(sub(&app).1, Tone::Bad);
    assert!(msg(&app).1);
    app.ki.runs.insert("lokal1".into(), 7);
    app.ki.tests.insert("lokal1".into(), Test::Running);
    app.ki_probe_msg(Msg { id: "lokal1".into(), run: 7, end: End::Late });
    assert!(sub(&app).0.contains("Keine Antwort nach 65 Sekunden"));
}
