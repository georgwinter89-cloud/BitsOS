use super::*;
use crate::assist_level::Level;
use serde_json::json;
use uilist::{Element, Role, Source};

fn win(title: &str, elements: Vec<Element>) -> Window {
    let mut w = Window { window: "w4".into(), app: "einstellungen".into(), title: title.into(), pid: 7, source: Source::Desktop, focused: true, rect: [0, 0, 800, 600], elements };
    w.number();
    w
}

fn step(e: &Element, action: Action, flag: Option<bool>) -> Step {
    Step { element: e.id.clone(), action, text: None, name: e.name.clone(), flag, why: String::new() }
}

fn page() -> Window {
    win(
        "Einstellungen",
        vec![
            Element::new(Role::Toggle, "Favoritenleiste", [0, 0, 10, 10]).key("fav").acts(&[Action::Toggle]),
            Element::new(Role::Toggle, "Ohne Vorschau an Claude senden: die Liste", [0, 20, 10, 10]).key("vor").acts(&[Action::Toggle]).asks(true),
            Element::new(Role::Button, "Löschen", [0, 40, 10, 10]).key("del").acts(&[Action::Press]),
            Element::new(Role::Button, "Anzeigen", [0, 60, 10, 10]).key("show").acts(&[Action::Press]),
            Element::secret("Passwort", [0, 80, 10, 10]).key("pw").acts(&[Action::Type]),
            Element::new(Role::Field, "Name", [0, 100, 10, 10]).key("name").acts(&[Action::Type]),
            Element::new(Role::Button, "Ja", [0, 120, 10, 10]).key("ja").acts(&[Action::Press]).settles(),
        ],
    )
}

fn el<'a>(w: &'a Window, name: &str) -> &'a Element {
    w.walk().into_iter().find(|e| e.name == name).unwrap()
}

#[test]
fn the_tool_arguments_become_a_step() {
    let s = parse(&json!({"element": "w4/a", "aktion": "type", "text": "Georg", "name": "Name", "unumkehrbar": false, "warum": "Der Name"})).unwrap();
    assert_eq!((s.action, s.text.as_deref(), s.flag, s.name.as_str()), (Action::Type, Some("Georg"), Some(false), "Name"));
    assert_eq!(parse(&json!({"element": "w4/a", "aktion": "press"})).unwrap().flag, None, "not said: unsure");
    assert!(parse(&json!({"aktion": "press"})).is_err());
    // Claude over the abo passed the id as the prompt writes it (14.11.): the step still names the window.
    for written in ["#w4/a", "[#w4/a]", " #w4/a "] {
        assert_eq!(parse(&json!({"element": written, "aktion": "press"})).unwrap().element, "w4/a", "{written}");
    }
    assert!(parse(&json!({"element": "[#]", "aktion": "press"})).is_err(), "nothing left is no element");
    assert!(parse(&json!({"element": "w4/a", "aktion": "fly"})).unwrap_err().contains("fly"));
}

#[test]
fn what_cannot_be_undone_in_german_and_english() {
    for n in ["Kaufen", "Jetzt kaufen", "Zahlungspflichtig bestellen", "Bezahlen", "Senden", "Absenden", "Veröffentlichen", "Posten", "Löschen", "Endgültig löschen", "Akzeptieren", "Alle akzeptieren", "Buy now", "Pay", "Send", "Publish", "Post", "Delete", "Submit", "Place order", "Accept", "Papierkorb leeren", "Wiederherstellen", "Alles vergessen …", "Ersetzen"] {
        assert!(final_words(n).is_some(), "{n}");
    }
    for n in ["Anzahl", "Postfach", "Speichern", "Weiter", "Abbrechen", "Favoritenleiste", "Design", "Senderliste öffnen"] {
        let hit = final_words(n);
        assert!(hit.is_none() || n == "Senderliste öffnen", "{n}: {hit:?}");
    }
}

#[test]
fn classification_counts_unsure_as_cannot_be_undone() {
    let w = page();
    assert_eq!(classify(&w, el(&w, "Favoritenleiste"), &step(el(&w, "Favoritenleiste"), Action::Toggle, None)), Risk::Normal, "a switch goes back");
    assert_eq!(classify(&w, el(&w, "Löschen"), &step(el(&w, "Löschen"), Action::Press, Some(false))), Risk::Final(DELETES), "the name beats the model's word");
    assert_eq!(classify(&w, el(&w, "Anzeigen"), &step(el(&w, "Anzeigen"), Action::Press, Some(false))), Risk::Normal);
    assert_eq!(classify(&w, el(&w, "Anzeigen"), &step(el(&w, "Anzeigen"), Action::Press, None)), Risk::Final(UNSURE), "the model did not say");
    assert_eq!(classify(&w, el(&w, "Anzeigen"), &step(el(&w, "Anzeigen"), Action::Press, Some(true))), Risk::Final(MODEL));
    let mut typed = step(el(&w, "Name"), Action::Type, Some(false));
    typed.text = Some("Georg\n".into());
    assert_eq!(classify(&w, el(&w, "Name"), &typed), Risk::Final(ENTER));
    typed.text = Some("Georg".into());
    assert_eq!(classify(&w, el(&w, "Name"), &typed), Risk::Normal);
    // "OK" in a dialog about deleting deletes; "OK" elsewhere does not.
    let dlg = win("Datei", vec![Element::new(Role::Text, "Soll die Datei wirklich gelöscht werden?", [0; 4]).key("t"), Element::new(Role::Button, "OK", [0; 4]).key("ok").acts(&[Action::Press])]);
    assert_eq!(classify(&dlg, el(&dlg, "OK"), &step(el(&dlg, "OK"), Action::Press, Some(false))), Risk::Final(DELETES));
    let plain = win("Info", vec![Element::new(Role::Text, "Fertig geladen.", [0; 4]).key("t"), Element::new(Role::Button, "OK", [0; 4]).key("ok").acts(&[Action::Press])]);
    assert_eq!(classify(&plain, el(&plain, "OK"), &step(el(&plain, "OK"), Action::Press, Some(false))), Risk::Normal);
    let acct = win("Mein Konto", vec![Element::new(Role::Toggle, "Newsletter", [0; 4]).key("n").acts(&[Action::Toggle])]);
    assert_eq!(classify(&acct, el(&acct, "Newsletter"), &step(el(&acct, "Newsletter"), Action::Toggle, Some(false))), Risk::Final(ACCOUNT));
    let terms = win("Anmeldung", vec![Element::new(Role::Toggle, "Ich akzeptiere die AGB", [0; 4]).key("agb").acts(&[Action::Toggle])]);
    assert_eq!(classify(&terms, el(&terms, "Ich akzeptiere die AGB"), &step(el(&terms, "Ich akzeptiere die AGB"), Action::Toggle, Some(false))), Risk::Final(TERMS));
}

#[test]
fn secret_fields_payment_data_and_confirm_settings_are_refused() {
    let w = page();
    assert!(hard_limit(el(&w, "Passwort"), Action::Type, Some("x")).unwrap_err().contains("nie"));
    for n in ["Kartennummer", "IBAN", "PIN", "Benutzername", "API-Schlüssel", "Sicherheitscode (CVC)"] {
        let f = Element::new(Role::Field, n, [0; 4]).acts(&[Action::Type]);
        assert!(hard_limit(&f, Action::Type, Some("1")).is_err(), "{n}");
    }
    assert!(hard_limit(el(&w, "Name"), Action::Type, Some("4111 1111 1111 1111")).unwrap_err().contains("Zahlungsdaten"));
    assert!(hard_limit(el(&w, "Name"), Action::Type, Some("DE89 3704 0044 0532 0130 00")).is_err());
    assert!(hard_limit(el(&w, "Name"), Action::Type, Some("Georg Winter, Tel. 0170 1234567")).is_ok());
    assert!(hard_limit(el(&w, "Ohne Vorschau an Claude senden: die Liste"), Action::Toggle, None).unwrap_err().contains("nur Georg"));
    assert!(hard_limit(el(&w, "Ja"), Action::Press, None).unwrap_err().contains("Rückfrage"));
    assert!(hard_limit(el(&w, "Favoritenleiste"), Action::Toggle, None).is_ok());
    // A button that opens its program's own question is asked, not refused.
    let b = Element::new(Role::Button, "Entfernen …", [0; 4]).acts(&[Action::Press]).asks(true);
    assert!(hard_limit(&b, Action::Press, None).is_ok());
    // ... but the "mehr" of a settings row that asks first is that setting.
    let row = win("Einstellungen", vec![Element::new(Role::Choice, "Riskanter Wert", [0; 4]).key("row").asks(true).child(Element::new(Role::Button, "mehr", [0; 4]).key("more").acts(&[Action::Press]).asks(true))]);
    assert!(limits(&row, el(&row, "mehr"), Action::Press, None).unwrap_err().contains("nur Georg"));
    assert!(limits(&w, &w.elements[2], Action::Press, None).is_ok(), "Löschen of a plain row");
}

fn decide(t: &mut Task, w: &Window, name: &str, a: Action, flag: Option<bool>, level: Level) -> (Next, Risk) {
    let e = el(w, name);
    t.decide(t.started, Some(w), Some(e), false, false, &step(e, a, flag), level)
}

/// A task of a question by Georg's own hand (`by_georg`) or from outside.
fn task(by_georg: bool) -> Task {
    Task { n: 1, started: Instant::now(), steps: 0, done: 0, by_georg, app: "bitscomp.einstellungen".into(), stopped: None }
}

#[test]
fn each_step_is_asked_at_fragt_vorher_run_in_the_automodus_and_refused_at_nur_antworten() {
    let w = page();
    let mut t = task(true);
    assert_eq!(decide(&mut t, &w, "Favoritenleiste", Action::Toggle, Some(false), Level::Ask).0, Next::Ask, "default: each step asked");
    assert_eq!(decide(&mut t, &w, "Favoritenleiste", Action::Toggle, Some(false), Level::Auto).0, Next::Run, "Automodus");
    assert_eq!(decide(&mut t, &w, "Löschen", Action::Press, Some(false), Level::Auto), (Next::Ask, Risk::Final(DELETES)), "asked even in Automodus");
    assert_eq!(decide(&mut t, &w, "Anzeigen", Action::Press, None, Level::Auto), (Next::Ask, Risk::Final(UNSURE)), "unsure: asked");
    assert_eq!(decide(&mut t, &w, "Löschen", Action::Press, Some(false), Level::Auto).0, Next::Ask, "F2 is fixed: no switch makes it unasked");
    assert!(matches!(decide(&mut t, &w, "Passwort", Action::Type, Some(false), Level::Auto).0, Next::Refuse(_)), "a secret field at every level");
    assert!(matches!(decide(&mut t, &w, "Favoritenleiste", Action::Toggle, Some(false), Level::Answer).0, Next::Refuse(s) if s == crate::db_act_app::ANSWER_ONLY), "Nur antworten");
    assert!(matches!(decide(&mut t, &w, "Ohne Vorschau an Claude senden: die Liste", Action::Toggle, Some(false), Level::Auto).0, Next::Refuse(_)));
    // A question from outside: the Automodus never runs a step unasked.
    let mut socket = task(false);
    assert_eq!(decide(&mut socket, &w, "Favoritenleiste", Action::Toggle, Some(false), Level::Auto).0, Next::Ask);
}

#[test]
fn budget_time_and_an_unexpected_screen_stop_the_task() {
    let w = page();
    let mut t = task(true);
    for _ in 0..STEP_BUDGET {
        assert_eq!(decide(&mut t, &w, "Favoritenleiste", Action::Toggle, Some(false), Level::Auto).0, Next::Run);
    }
    assert!(matches!(decide(&mut t, &w, "Favoritenleiste", Action::Toggle, Some(false), Level::Auto).0, Next::Stop(s) if s.contains("30 Schritte")));
    let mut t = task(true);
    let e = el(&w, "Favoritenleiste");
    let late = t.started + TIME_LIMIT + Duration::from_secs(1);
    assert!(matches!(t.decide(late, Some(&w), Some(e), false, false, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Stop(s) if s.contains("Minuten")));
    let mut t = task(true);
    let mut renamed = step(e, Action::Toggle, Some(false));
    renamed.name = "Statusleiste".into();
    assert!(matches!(t.decide(t.started, Some(&w), Some(e), false, false, &renamed, Level::Auto).0, Next::Stop(s) if s.contains("anders")));
    assert!(matches!(t.decide(t.started, Some(&w), None, false, false, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Stop(_)), "the element is gone");
    assert!(matches!(t.decide(t.started, None, None, false, false, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Stop(_)), "the window is gone");
    t.stopped = Some("Esc".into());
    assert_eq!(t.decide(t.started, Some(&w), Some(e), false, false, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Stop("Esc".into()), "after a stop nothing runs");
}

#[test]
fn any_programs_window_but_never_the_assistants_own_buttons_or_a_never_see_program() {
    let w = page();
    let mut t = task(true);
    let e = el(&w, "Favoritenleiste");
    assert!(matches!(t.decide(t.started, Some(&w), Some(e), true, false, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Refuse(s) if s.contains("nur Georg")));
    let mut other = page();
    other.window = "w9".into();
    other.app = "brave".into();
    assert_eq!(t.decide(t.started, Some(&other), Some(e), false, false, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Run, "another program's window too (EINS)");
    assert!(matches!(t.decide(t.started, Some(&other), Some(e), false, true, &step(e, Action::Toggle, Some(false)), Level::Auto).0, Next::Refuse(s) if s.contains(crate::assist_level::NOT_SEEN)));
}

#[test]
fn a_step_in_plain_german_without_secret_text() {
    let w = page();
    let mut s = step(el(&w, "Name"), Action::Type, Some(false));
    s.text = Some("Georg".into());
    assert_eq!(describe(el(&w, "Name"), &s), "in „Name“ „Georg“ tippen");
    assert_eq!(describe(el(&w, "Favoritenleiste"), &step(el(&w, "Favoritenleiste"), Action::Toggle, None)), "„Favoritenleiste“ einschalten");
    assert_eq!(describe(el(&w, "Löschen"), &step(el(&w, "Löschen"), Action::Press, None)), "„Löschen“ drücken");
    let mut pw = step(el(&w, "Passwort"), Action::Type, None);
    pw.text = Some("geheim".into());
    assert!(!describe(el(&w, "Passwort"), &pw).contains("geheim"));
}

#[test]
fn a_step_is_judged_by_what_its_click_does() {
    // The sofa's "Spiel beenden" asks itself on a press; a focus there is the same click.
    let sofa = win("Sofa", vec![
        Element::new(Role::Button, "Spiel beenden", [0; 4]).key("q:end").acts(&[Action::Press, Action::Focus]).asks(true),
        Element::new(Role::Button, "Weiterspielen", [0; 4]).key("q:resume").acts(&[Action::Press, Action::Focus]),
    ]);
    let mut t = task(true);
    assert_eq!(decide(&mut t, &sofa, "Spiel beenden", Action::Focus, Some(false), Level::Auto), (Next::Ask, Risk::Final(ASKS_ITSELF)), "asked in the Automodus");
    assert_eq!(classify(&sofa, el(&sofa, "Weiterspielen"), &step(el(&sofa, "Weiterspielen"), Action::Focus, None)), Risk::Final(UNSURE), "the unsure rule holds for a focus click");
    assert_eq!(classify(&sofa, el(&sofa, "Weiterspielen"), &step(el(&sofa, "Weiterspielen"), Action::Choose, None)), Risk::Final(UNSURE));
    // A foreign program's focus only moves the focus.
    let mut foreign = sofa.clone();
    foreign.source = Source::Atspi;
    assert_eq!(classify(&foreign, el(&foreign, "Weiterspielen"), &step(el(&foreign, "Weiterspielen"), Action::Focus, None)), Risk::Normal);
    // An option chosen by its words is judged by them.
    let combo = win("Datei", vec![Element::new(Role::Choice, "Aktion", [0; 4]).key("c").acts(&[Action::Choose])]);
    let mut foreign = combo.clone();
    foreign.source = Source::Atspi;
    let mut s = step(el(&foreign, "Aktion"), Action::Choose, Some(false));
    s.text = Some("Endgültig löschen".into());
    assert_eq!(classify(&foreign, el(&foreign, "Aktion"), &s), Risk::Final(DELETES));
    s.text = Some("Löschen".into());
    assert_eq!(classify(&foreign, el(&foreign, "Aktion"), &s), Risk::Final(DELETES));
    s.text = Some("Blau".into());
    assert_eq!(classify(&foreign, el(&foreign, "Aktion"), &s), Risk::Normal);
}

#[test]
fn more_wordings_that_cannot_be_undone() {
    for n in ["Payment", "Proceed to payment", "Mit PayPal zahlen", "Überweisen", "Überweisung ausführen", "Transfer", "Jetzt buchen", "Book now", "Spenden", "Donate", "Abonnement abschließen", "Abo abschließen", "Kostenpflichtig abschließen", "Unterschreiben", "Sign", "Nicht speichern", "Don't Save", "Dont save", "Close without saving", "Schließen ohne Speichern"] {
        assert!(final_words(n).is_some(), "{n}");
    }
    for n in ["Speichern", "Save", "Über BitsOS", "About", "Abbrechen", "Weiter"] {
        assert!(final_words(n).is_none(), "{n}: {:?}", final_words(n));
    }
    let w = win("Einstellungen", vec![
        Element::new(Role::Button, "Passwort ändern", [0; 4]).key("pw").acts(&[Action::Press]),
        Element::new(Role::Button, "Change password", [0; 4]).key("pw2").acts(&[Action::Press]),
        Element::new(Role::Button, "E-Mail-Adresse ändern", [0; 4]).key("mail").acts(&[Action::Press]),
    ]);
    for n in ["Passwort ändern", "Change password", "E-Mail-Adresse ändern"] {
        assert_eq!(classify(&w, el(&w, n), &step(el(&w, n), Action::Press, Some(false))), Risk::Final(ACCOUNT), "{n}");
    }
}

#[test]
fn typing_over_what_a_field_holds_is_asked() {
    let w = win("Brief", vec![
        Element::new(Role::Field, "Text", [0; 4]).key("full").acts(&[Action::Type]).value("Lieber Georg, …"),
        Element::new(Role::Field, "Leer", [0; 4]).key("empty").acts(&[Action::Type]).value(""),
        Element::new(Role::Field, "Dabei", [0; 4]).key("edit").acts(&[Action::Type]).value("Hallo").state_if(State::Focused, true),
        Element::new(Role::Field, "Ohne Wert", [0; 4]).key("none").acts(&[Action::Type]),
    ]);
    let typed = |w: &Window, n: &str| {
        let mut s = step(el(w, n), Action::Type, Some(false));
        s.text = Some("neu".into());
        classify(w, el(w, n), &s)
    };
    assert_eq!(typed(&w, "Text"), Risk::Final(DELETES), "the text it held is gone");
    assert_eq!(typed(&w, "Leer"), Risk::Normal);
    assert_eq!(typed(&w, "Dabei"), Risk::Normal, "a field being edited gets the text at the cursor");
    assert_eq!(typed(&w, "Ohne Wert"), Risk::Normal);
    let mut foreign = w.clone();
    foreign.source = Source::Atspi;
    assert_eq!(typed(&foreign, "Dabei"), Risk::Final(DELETES), "atspid sets the whole text");
    assert_eq!(typed(&foreign, "Ohne Wert"), Risk::Final(UNSURE), "a foreign field whose text is not known");
    assert_eq!(typed(&foreign, "Leer"), Risk::Normal);
}

#[test]
fn typing_into_a_short_line_of_a_foreign_program_runs_in_the_automodus() {
    // EINGABE (Georg 14.11.): Brave's address bar and Google's search box.
    let long = "x".repeat(SHORT + 1);
    let mut w = win("Neuer Tab - Brave", vec![
        Element::new(Role::Field, "Adress- und Suchleiste", [0; 4]).key("url").acts(&[Action::Type]).value("https://example.org/").state_if(State::Singleline, true),
        Element::new(Role::Choice, "Suche", [0; 4]).key("q").acts(&[Action::Press, Action::Type]).value("").state_if(State::Singleline, true),
        // Google's box is a <textarea role=combobox>: Chromium says several lines.
        Element::new(Role::Choice, "Google leer", [0; 4]).key("g0").acts(&[Action::Press, Action::Type]).value("").state_if(State::Multiline, true),
        Element::new(Role::Choice, "Google mit Suche", [0; 4]).key("g1").acts(&[Action::Press, Action::Type]).value("wetter").state_if(State::Multiline, true),
        Element::new(Role::Field, "Lang", [0; 4]).key("long").acts(&[Action::Type]).value(long.as_str()).state_if(State::Singleline, true),
        Element::new(Role::Field, "Kommentar", [0; 4]).key("multi").acts(&[Action::Type]).value("Ein Satz.").state_if(State::Multiline, true),
        Element::new(Role::Field, "Leerer Kommentar", [0; 4]).key("multi0").acts(&[Action::Type]).value("").state_if(State::Multiline, true),
        Element::new(Role::Field, "Unlesbar", [0; 4]).key("none").acts(&[Action::Type]).state_if(State::Singleline, true),
        Element::new(Role::Field, "Ohne Zeilenangabe", [0; 4]).key("plain").acts(&[Action::Type]).value("kurz"),
    ]);
    w.source = Source::Atspi;
    let typed = |n: &str, t: &str| {
        let mut s = step(el(&w, n), Action::Type, Some(false));
        s.text = Some(t.into());
        classify(&w, el(&w, n), &s)
    };
    assert_eq!(typed("Adress- und Suchleiste", "such nach Georg"), Risk::Normal, "one line, a URL: runs");
    assert_eq!(typed("Suche", "Georg"), Risk::Normal, "an empty search box");
    assert_eq!(typed("Google leer", "Georg"), Risk::Normal, "Google's start page: its box is empty");
    assert_eq!(typed("Google mit Suche", "Georg"), Risk::Final(DELETES), "a results page: several lines with content, asked");
    assert_eq!(typed("Lang", "Georg"), Risk::Final(DELETES), "more than 200 characters would be gone");
    assert_eq!(typed("Kommentar", "Georg"), Risk::Final(DELETES), "several lines with content: asked");
    assert_eq!(typed("Leerer Kommentar", "Georg"), Risk::Normal, "empty is empty");
    assert_eq!(typed("Unlesbar", "Georg"), Risk::Final(UNSURE), "a text that cannot be read may be anything");
    assert_eq!(typed("Ohne Zeilenangabe", "Georg"), Risk::Final(DELETES), "not known to be one line: as several");
    assert_eq!(typed("Leerer Kommentar", "Zeile 1\nZeile 2"), Risk::Final(ENTER), "a line break is asked");
    let mut task = task(true);
    let mut s = step(el(&w, "Adress- und Suchleiste"), Action::Type, None);
    s.text = Some("https://www.google.com".into());
    let (next, _) = task.decide(Instant::now(), Some(&w), Some(el(&w, "Adress- und Suchleiste")), false, false, &s, Level::Auto);
    assert_eq!(next, Next::Run, "the Automodus: an address bar's URL runs unasked");
    let mut s = step(el(&w, "Kommentar"), Action::Type, None);
    s.text = Some("Georg".into());
    assert_eq!(task.decide(Instant::now(), Some(&w), Some(el(&w, "Kommentar")), false, false, &s, Level::Auto).0, Next::Ask, "still asked");
}

#[test]
fn no_enter_in_a_one_line_field_and_no_more_than_2000_characters() {
    let url = Element::new(Role::Field, "Adresse", [0; 4]).key("url").acts(&[Action::Type]).state_if(State::Singleline, true);
    assert!(hard_limit(&url, Action::Type, Some("google.com\n")).unwrap_err().contains("Enter"), "a line break would send the form");
    assert!(hard_limit(&url, Action::Type, Some("google.com")).is_ok());
    let text = Element::new(Role::Field, "Text", [0; 4]).key("t").acts(&[Action::Type]).state_if(State::Multiline, true);
    assert!(hard_limit(&text, Action::Type, Some("a\nb")).is_ok(), "several lines: asked, not refused");
    let long = "x".repeat(MAX_TEXT + 1);
    assert!(parse(&json!({"element": "w4/a", "aktion": "type", "text": long})).unwrap_err().contains("2000"));
    assert!(parse(&json!({"element": "w4/a", "aktion": "type", "text": "x".repeat(MAX_TEXT)})).is_ok());
}

#[test]
fn no_key_of_a_secret_entry_is_pressed() {
    let key = Element::new(Role::Button, "a", [0; 4]).key("key:a").acts(&[Action::Press, Action::Focus]).state_if(State::Secret, true);
    for a in [Action::Press, Action::Focus, Action::Choose, Action::Toggle] {
        assert!(hard_limit(&key, a, None).unwrap_err().contains("geheim"), "{a:?}");
    }
    assert!(hard_limit(&key, Action::Type, Some("x")).is_err());
    let field = Element::secret("Eingabe", [0; 4]).key("search-field").acts(&[Action::Type, Action::Focus]);
    assert!(hard_limit(&field, Action::Focus, None).is_ok(), "going into the field: Georg types himself");
    assert!(hard_limit(&field, Action::Type, Some("x")).is_err());
    // In the Automodus with the model's "no" too.
    let sofa = win("Sofa", vec![key]);
    let mut t = task(true);
    assert!(matches!(decide(&mut t, &sofa, "a", Action::Press, Some(false), Level::Auto).0, Next::Refuse(_)));
}

#[test]
fn abschicken_runs_in_the_automodus_only_in_an_address_bar_or_a_search_box() {
    // EINGABE2 (Georg 14.11.): Enter in the address bar and in search fields unasked, everywhere else asked.
    let field = |name: &str, key: &str| Element::new(Role::Field, name, [0; 4]).key(key).acts(&[Action::Type, Action::Submit]).value("");
    let mut w = win("Google - Brave", vec![
        field("Adress- und Suchleiste", "url").state_if(State::Singleline, true),
        Element::new(Role::Choice, "Suche", [0; 4]).key("q").acts(&[Action::Type, Action::Submit]).value("").state_if(State::Multiline, true).state_if(State::Search, true),
        field("Begriff", "s").state_if(State::Singleline, true).state_if(State::Search, true),
        field("Kommentar", "c").state_if(State::Multiline, true),
        field("Suche nach Freunden", "multi").state_if(State::Multiline, true),
        field("Vorname", "n").state_if(State::Singleline, true),
        field("Adresse", "addr").state_if(State::Singleline, true),
        field("E-Mail-Adresse", "mail").state_if(State::Singleline, true),
        Element::secret("Passwort", [0; 4]).key("pw").acts(&[Action::Type]),
        Element::new(Role::Button, "Google Suche", [0; 4]).key("b").acts(&[Action::Press]),
    ]);
    w.source = Source::Atspi;
    let risk = |w: &Window, n: &str| classify(w, el(w, n), &step(el(w, n), Action::Submit, Some(false)));
    assert_eq!(risk(&w, "Adress- und Suchleiste"), Risk::Normal, "the address bar by its name, one line");
    assert_eq!(risk(&w, "Suche"), Risk::Normal, "Google's box by its semantics, though it is several lines");
    assert_eq!(risk(&w, "Begriff"), Risk::Normal, "a search box by its semantics alone");
    assert_eq!(risk(&w, "Kommentar"), Risk::Final(SENDS), "a comment is sent");
    assert_eq!(risk(&w, "Suche nach Freunden"), Risk::Final(SENDS), "a name alone is not enough for several lines");
    assert_eq!(risk(&w, "Vorname"), Risk::Final(SENDS));
    assert_eq!(risk(&w, "Adresse"), Risk::Final(SENDS), "a form's address is no address bar");
    let mut task = task(true);
    let go = |task: &mut Task, n: &str| task.decide(Instant::now(), Some(&w), Some(el(&w, n)), false, false, &step(el(&w, n), Action::Submit, None), Level::Auto).0;
    assert_eq!(go(&mut task, "Adress- und Suchleiste"), Next::Run, "the Automodus runs it");
    assert_eq!(go(&mut task, "Kommentar"), Next::Ask, "asked although the Automodus is on");
    assert!(matches!(go(&mut task, "E-Mail-Adresse"), Next::Refuse(_)), "never in a log-in's field");
    assert!(matches!(go(&mut task, "Passwort"), Next::Refuse(_)));
    assert!(hard_limit(el(&w, "Google Suche"), Action::Submit, None).is_err(), "only in a field");
    let mut shop = win("Kasse - Jetzt kaufen - Brave", w.elements.clone());
    shop.source = Source::Atspi;
    assert_eq!(risk(&shop, "Suche"), Risk::Final(BUYS), "a window that speaks of buying: asked even for a search");
    let mut konto = win("Mein Konto - Brave", w.elements.clone());
    konto.source = Source::Atspi;
    assert_eq!(risk(&konto, "Adress- und Suchleiste"), Risk::Final(ACCOUNT));
    // Checkers EINGABE2: a composer with suggestions is no search box; "senden" must not hide "kaufen"; the
    // account rule reads the window's words too.
    let mut chat = win("Chat - Brave", vec![
        Element::new(Role::Choice, "Nachricht", [0; 4]).key("m").acts(&[Action::Type, Action::Submit]).value("").state_if(State::Multiline, true).state_if(State::Search, true),
        Element::new(Role::Choice, "Empfänger", [0; 4]).key("to").acts(&[Action::Type, Action::Submit]).value("").state_if(State::Singleline, true).state_if(State::Search, true),
        field("Text", "t").state_if(State::Multiline, true).state_if(State::Search, true),
    ]);
    chat.source = Source::Atspi;
    assert_eq!(risk(&chat, "Nachricht"), Risk::Final(SENDS), "a multi-line combobox with suggestions sends");
    assert_eq!(risk(&chat, "Empfänger"), Risk::Final(SENDS), "a combobox needs the name too");
    assert_eq!(risk(&chat, "Text"), Risk::Final(SENDS), "several lines in a search form, not named so");
    let mut both = w.elements.clone();
    both.insert(0, Element::new(Role::Text, "Newsletter senden", [0; 4]));
    both.insert(1, Element::new(Role::Heading, "Jetzt kaufen", [0; 4]));
    let mut shop2 = win("Angebote - Brave", both.clone());
    shop2.source = Source::Atspi;
    assert_eq!(risk(&shop2, "Suche"), Risk::Final(BUYS), "the buying words after the sending ones count");
    both[1] = Element::new(Role::Heading, "Ihr Konto", [0; 4]);
    let mut acc = win("Einstellungen - Brave", both);
    acc.source = Source::Atspi;
    assert_eq!(risk(&acc, "Suche"), Risk::Final(ACCOUNT), "an account by the window's words");
    assert_eq!(parse(&json!({"element": "w4/a", "aktion": "abschicken", "name": "Suche", "warum": "Suchen."})).unwrap().action, Action::Submit, "the tool's word");
    assert_eq!(describe(el(&w, "Suche"), &step(el(&w, "Suche"), Action::Submit, None)), "in „Suche“ Enter drücken (abschicken)");
}
