//! The page "KI" after the incident of 12.11.: helpers offered by name, the chat's provider asked before its
//! kind or address changes, refusals that say what to do, a line beside every kind.

use mcpreg::Json;

use super::tests::{PID, answer, click, msg, new_app, sent, set, with_lokal1, yes};
use crate::helpers::Helper;
use crate::input::Origin;
use crate::ki;
use crate::page::Kind;
use crate::page_draw::Tone;
use crate::settings_write::Req;

fn helper(path: &str) -> Helper {
    Helper { id: "abo".into(), name: "Mein Abo-Helfer".into(), about: "Antwortet über ein Programm.".into(), path: path.into(), place: Some("cloud".into()), models: Vec::new() }
}

/// A program file the helper's check accepts (the test binary itself).
fn exe() -> String {
    std::env::current_exe().expect("test binary").display().to_string()
}

fn sub(app: &crate::state::App, key: &str) -> (String, Tone) {
    let b = &app.ki.built;
    b.subs[b.row_of(key).unwrap_or_else(|| panic!("no row {key}"))].clone().expect("a sentence")
}

#[test]
fn an_installed_helper_is_offered_by_name_and_taken_with_one_click() {
    let (_d, mut app, rx) = new_app();
    assert!(app.ki.built.row_of(crate::ki_helpers::RESCAN).is_some(), "the section is there with no helper too");
    app.ki.helpers = vec![helper(&exe())];
    app.ki_refresh();
    let r = app.ki.built.row_of("ki.helper.abo").expect("the helper's row");
    assert_eq!(app.ki.built.model.rows[r].label, "Hilfsprogramm: Mein Abo-Helfer");
    // Georg's broken provider of the same name stays; the helper becomes abo-2.
    set(&mut app, &[("ai.providers", "abo"), ("ai.provider.abo.kind", "anthropic"), ("ai.provider.abo.address", "abo")]);
    click(&mut app, "button=ki.helper.abo=adopt", Origin::Injected);
    assert_eq!(sent(&rx), [Req::set("ai.providers", Json::str("abo, abo-2"))]);
    answer(&mut app, "ai.providers", true, "Geändert");
    assert_eq!(sent(&rx), [Req::set("ai.provider.abo-2.kind", Json::str("program"))]);
    answer(&mut app, "ai.provider.abo-2.kind", true, "Geändert");
    assert_eq!(sent(&rx), [Req::set("ai.provider.abo-2.address", Json::str(&exe()))]);
    answer(&mut app, "ai.provider.abo-2.address", true, "Geändert");
    assert_eq!(sent(&rx), [Req::set("ai.provider.abo-2.place", Json::str("cloud"))]);
    answer(&mut app, "ai.provider.abo-2.place", true, "Geändert");
    let line = msg(&app).0;
    assert!(line.starts_with("„Mein Abo-Helfer“ ist jetzt als Anbieter „abo-2“ eingetragen.") && line.contains("„Fürs Gespräch nehmen“"), "{line}");
    assert!(!line.contains("Ort"), "the place came from the description: {line}");
    set(&mut app, &[("ai.providers", "abo, abo-2"), ("ai.provider.abo-2.kind", "program"), ("ai.provider.abo-2.address", &exe())]);
    // Now it is offered for the chat; with nobody there yet, one click.
    click(&mut app, "button=ki.helper.abo=chat", Origin::Injected);
    assert_eq!(sent(&rx), [Req::set("ai.role.chat", Json::str("abo-2"))]);
    set(&mut app, &[("ai.role.chat", "abo-2")]);
    let r = app.ki.built.row_of("ki.helper.abo").expect("row");
    assert!(matches!(&app.ki.built.model.rows[r].kind, Kind::Fixed(t) if t == "antwortet im Gespräch"));
    // Another one answers: asked first, and only Georg's own yes switches.
    set(&mut app, &[("ai.role.chat", "abo")]);
    click(&mut app, "button=ki.helper.abo=chat", Origin::Physical);
    assert!(sent(&rx).is_empty() && app.pages.by_pid[&PID].confirm.as_ref().is_some_and(|d| d.title == "Im Gespräch wechseln?"));
    yes(&mut app, Origin::Physical);
    assert_eq!(sent(&rx), [Req::Guarded { key: "ai.role.chat".into(), value: Json::str("abo-2"), whilst: vec![("ai.role.chat".into(), Json::str("abo"))] }]);
    // A program that went away is not added.
    app.ki.helpers = vec![helper("/fehlt/ganz")];
    app.ki_refresh();
    click(&mut app, "button=ki.helper.abo=adopt", Origin::Injected);
    assert!(sent(&rx).is_empty() && msg(&app).1 && msg(&app).0.contains("Neu suchen"), "{:?}", msg(&app));
    assert!(app.page_report().contains("helpers=abo"));
}

#[test]
fn the_chats_provider_changes_kind_or_address_only_after_a_yes_and_says_how_to_undo() {
    let (_d, mut app, rx) = new_app();
    with_lokal1(&mut app);
    // Not the chat's: at once, and the line names Rückgängig.
    click(&mut app, "choice=ai.provider.lokal1.kind=anthropic", Origin::Injected);
    assert_eq!(sent(&rx), [Req::set("ai.provider.lokal1.kind", Json::str("anthropic"))]);
    answer(&mut app, "ai.provider.lokal1.kind", true, "Geändert");
    assert_eq!(msg(&app).0, "Art von „lokal1“ ist jetzt „Anthropic“. „Rückgängig“ oben rechts holt „OpenAI-kompatibel“ zurück.");
    // The chat's provider (here as its fallback): the dialog first, nothing sent.
    set(&mut app, &[("ai.role.chat_fallback", "lokal1")]);
    click(&mut app, "choice=ai.provider.lokal1.kind=program", Origin::Physical);
    assert!(sent(&rx).is_empty());
    let d = app.pages.by_pid[&PID].confirm.clone().expect("a dialog");
    assert_eq!(d.title, "Art von „lokal1“ ändern?");
    assert!(d.lines.iter().any(|l| l == "Jetzt: OpenAI-kompatibel") && d.lines.iter().any(|l| l == "Neu: Hilfsprogramm"), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.contains("„Rückgängig“")) && d.lines.iter().any(|l| l.contains("ganzen Pfad")), "{:?}", d.lines);
    app.page_act(PID, crate::page::Part::DialogCancel);
    assert!(sent(&rx).is_empty() && msg(&app).0.contains("nicht geändert (abgebrochen)"));
    // Injected input cannot say yes to it.
    click(&mut app, "choice=ai.provider.lokal1.kind=program", Origin::Injected);
    yes(&mut app, Origin::Injected);
    assert!(sent(&rx).is_empty());
    click(&mut app, "choice=ai.provider.lokal1.kind=program", Origin::Physical);
    yes(&mut app, Origin::Physical);
    let whilst = vec![("ai.provider.lokal1.kind".to_string(), Json::str("openai_compatible"))];
    assert_eq!(sent(&rx), [Req::Guarded { key: "ai.provider.lokal1.kind".into(), value: Json::str("program"), whilst }]);
    answer(&mut app, "ai.provider.lokal1.kind", true, "Geändert");
    assert!(msg(&app).0.contains("holt „OpenAI-kompatibel“ zurück"), "{:?}", msg(&app));
    // The address typed into the field: the same question.
    set(&mut app, &[("ai.role.chat", "lokal1")]);
    click(&mut app, "field=ai.provider.lokal1.address", Origin::Physical);
    let r = app.ki.built.row_of("ai.provider.lokal1.address").expect("row");
    app.ki_commit(PID, r, "abo".into());
    assert!(sent(&rx).is_empty());
    let d = app.pages.by_pid[&PID].confirm.clone().expect("a dialog");
    assert_eq!(d.title, "Adresse von „lokal1“ ändern?");
    assert!(d.lines[0].contains("antwortet gerade"), "{:?}", d.lines);
}

#[test]
fn every_kind_has_its_line_and_a_wrong_address_says_what_to_do() {
    let (_d, mut app, _rx) = new_app();
    let kinds = |app: &crate::state::App, key: &str| match &app.ki.built.model.rows[app.ki.built.row_of(key).expect("row")].kind {
        Kind::Pills { opts, notes, .. } => (opts.len(), notes.clone()),
        k => panic!("{k:?}"),
    };
    let (n, notes) = kinds(&app, ki::ADD_KIND);
    assert_eq!(notes.len(), n);
    assert!(notes.iter().any(|l| l.contains("Abo")), "{notes:?}");
    set(&mut app, &[("ai.providers", "abo"), ("ai.provider.abo.kind", "anthropic"), ("ai.provider.abo.address", "/data/home/1000/helfer")]);
    assert_eq!(kinds(&app, "ai.provider.abo.kind").1.len(), kinds(&app, "ai.provider.abo.kind").0);
    let (text, tone) = sub(&app, "ai.provider.abo.address");
    assert_eq!(tone, Tone::Bad);
    assert!(text.contains("„Hilfsprogramm“ wählen"), "{text}");
    set(&mut app, &[("ai.provider.abo.address", "https://api.anthropic.com")]);
    assert_eq!(sub(&app, "ai.provider.abo.address").1, Tone::Plain);
}
