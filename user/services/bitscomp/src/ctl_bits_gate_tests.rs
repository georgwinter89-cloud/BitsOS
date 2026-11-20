use smithay::reexports::wayland_server::Display;
use uilist::Role;

use super::*;

fn app() -> (Display<App>, App) {
    let d = Display::<App>::new().expect("display");
    let a = App::new(d.handle(), "bits-gate-test".into(), (2560, 1440));
    (d, a)
}

/// A desktop button's id ("Programme": pressing it opens the library, a Normal step).
fn programme(app: &App) -> String {
    let w = app.ui_windows(&Full::One(crate::ui_reg::DESKTOP.into())).into_iter().find(|w| w.window == crate::ui_reg::DESKTOP).unwrap();
    w.walk().into_iter().find(|e| e.role == Role::Button && e.name == "Programme").unwrap().id.clone()
}

fn judged(app: &App, line: &str) -> Verdict {
    let words: Vec<&str> = line.split_whitespace().collect();
    judge(app, &words, line)
}

#[test]
fn a_step_follows_the_level() {
    let (_d, mut app) = app();
    let line = format!("ui-act {} press", programme(&app));
    assert!(matches!(judged(&app, &line), Verdict::Ask(t) if t.contains("„Programme“ drücken")), "ask is the default");
    app.settings.set("ai.level", "answer");
    assert_eq!(judged(&app, &line), Verdict::Refuse(ANSWER_ONLY.into()));
    app.settings.set("ai.level", "auto");
    assert!(matches!(judged(&app, &line), Verdict::Ask(t) if t.ends_with("ist nicht sicher.")), "a press nobody called undoable is asked at every level");
    assert_eq!(judged(&app, &format!("bits-schritt rueckgaengig {line}")), Verdict::Pass);
    assert!(matches!(judged(&app, &format!("bits-schritt endgueltig {line}")), Verdict::Ask(_)));
    assert!(matches!(judged(&app, "bits-schritt rueckgaengig close 3"), Verdict::Refuse(_)));
    app.settings.set("ai.level", "answer");
    assert_eq!(judged(&app, &format!("bits-schritt rueckgaengig {line}")), Verdict::Refuse(ANSWER_ONLY.into()));
    assert_eq!(judged(&app, &format!("bits-schritt endgueltig {line}")), Verdict::Refuse(ANSWER_ONLY.into()), "nothing acts at Nur antworten");
    app.settings.set("ai.level", "ask");
    let (Verdict::Ask(fin), Verdict::Ask(plain)) = (judged(&app, &format!("bits-schritt endgueltig {line}")), judged(&app, &format!("bits-schritt rueckgaengig {line}"))) else {
        panic!("both are asked at Fragt vorher")
    };
    assert!(fin.starts_with(&plain) && fin.len() > plain.len(), "the final one says why: {fin:?} / {plain:?}");
    assert!(matches!(judged(&app, "program-start Firefox"), Verdict::Ask(t) if t == "Firefox starten."));
    assert!(matches!(judged(&app, "bits-clip-set /x"), Verdict::Ask(_)));
}

#[test]
fn reading_runs_and_everything_unlisted_is_refused() {
    let (_d, mut app) = app();
    app.settings.set("ai.level", "answer");
    for l in ["list", "ui", "ui desktop", "ui text", "program-find foot", "bits-clip-get", "bits-antwort 1", "notice", "stats", "library search a"] {
        assert_eq!(judged(&app, l), Verdict::Pass, "{l}");
    }
    for l in ["set ai.level auto", "key 28", "click 1 1", "spawn /bin/sh", "notice clear", "stats reset", "phys key 28", "assist ask hallo", "bits-fragen"] {
        assert!(matches!(judged(&app, l), Verdict::Refuse(_)), "{l}");
    }
    assert_eq!(judged(&app, "bits-frage leeren 1 10"), Verdict::Refuse(NOT_FROM_TREE.into()));
    assert_eq!(judged(&app, "bits-frage einstellung a b"), Verdict::Pass);
    assert_eq!(judged(&app, "bits-mit 3 ui-act desktop/x press"), Verdict::Pass, "its own check is take_yes");
}

#[test]
fn a_never_seen_program_is_neither_read_nor_acted_in() {
    let (_d, mut app) = app();
    app.settings.set("ai.level", "auto");
    app.settings.set("assistant.never_see", "Bitscomp, keepass");
    assert_eq!(judged(&app, "ui desktop"), Verdict::Refuse(NEVER_SEE.into()), "the desktop's program is bitscomp");
    let line = format!("ui-act {} press", programme(&app));
    assert_eq!(judged(&app, &line), Verdict::Refuse(NEVER_SEE.into()));
    assert_eq!(recheck(&app, &line), Err(NEVER_SEE.into()));
}

#[test]
fn only_georgs_hand_says_yes_and_a_yes_runs_its_line_once() {
    let (_d, mut app) = app();
    let n = open(&mut app, 42, Some("ui-act desktop/x press".into()), "x drücken.".into(), true);
    assert_eq!(status(n), "wartet");
    assert!(pending().iter().any(|q| q.n == n));
    assert!(app.desk.notice.as_ref().is_some_and(|(t, _)| t == "Der Assistent möchte x drücken."));
    assert_eq!(answer(&mut app, n, true, Origin::Injected), Err(REAL_ONLY.into()));
    assert_eq!(status(n), "wartet");
    assert!(take_yes(n, 42, "ui-act desktop/x press").is_err(), "no yes yet");
    answer(&mut app, n, true, Origin::Physical).unwrap();
    assert_eq!(status(n), "ja");
    assert!(app.desk.notice.is_none(), "the question's notice goes with the answer");
    assert!(take_yes(n, 43, "ui-act desktop/x press").is_err(), "another asker");
    assert!(take_yes(n, 42, "ui-act desktop/y press").is_err(), "another line");
    take_yes(n, 42, "ui-act desktop/x press").unwrap();
    assert!(take_yes(n, 42, "ui-act desktop/x press").is_err(), "once");
    let m = open(&mut app, 42, None, "y.".into(), false);
    answer(&mut app, m, false, Origin::Injected).unwrap();
    assert_eq!(status(m), "nein", "anyone may say no");
    assert_eq!(status(9999), "unbekannt");
}

#[test]
fn questions_name_what_happens() {
    assert_eq!(reason("kauft oder bezahlt etwas"), "Das kauft oder bezahlt etwas.");
    assert_eq!(reason("der Zeilenumbruch kann ein Formular absenden"), "Der Zeilenumbruch kann ein Formular absenden.");
    assert_eq!(frage_text(&["bits-frage", "einstellung", "desktop.theme", "dunkel"]).as_deref(), Some("die Einstellung desktop.theme auf „dunkel“ setzen."));
    assert_eq!(frage_text(&["bits-frage", "zurueck", "3", "/data/a b"]).as_deref(), Some("„/data/a b“ aus dem Papierkorb zurücklegen."));
    assert!(frage_text(&["bits-frage", "leeren", "2", "2097152"]).unwrap().contains("2 Einträge, 2.0 MB"));
    assert_eq!(frage_text(&["bits-frage", "kaufen"]), None);
}

/// EINS-B's rule for the tree (once `assist_tree::rule`), now judged by this one gate: reading at every level, a
/// change by the level, Georg's own hand never.
#[test]
fn the_tree_follows_the_level_and_reading_is_free() {
    let (_d, mut app) = app();
    for l in ["answer", "ask", "auto"] {
        app.settings.set("ai.level", l);
        for r in ["ui focus", "durchblick state", "assist state", "list", "page", "cursor"] {
            assert_eq!(judged(&app, r), Verdict::Pass, "{r} at {l}");
        }
        assert!(matches!(judged(&app, "phys key Return"), Verdict::Refuse(_)), "never Georg's own hand ({l})");
        assert!(matches!(judged(&app, "key 28"), Verdict::Refuse(_)), "raw input is no step the gate can judge ({l})");
    }
    app.settings.set("ai.level", "answer");
    assert_eq!(judged(&app, "program-start foot"), Verdict::Refuse(ANSWER_ONLY.into()));
    app.settings.set("ai.level", "ask");
    assert!(matches!(judged(&app, "close 3"), Verdict::Ask(_)), "asked at Fragt vorher");
    app.settings.set("ai.level", "auto");
    assert_eq!(judged(&app, "close 3"), Verdict::Pass);
}

/// PLAN K3: while the service runs only questions from outside, the tree gets no more than ask.
#[test]
fn a_question_from_outside_gives_the_tree_no_more_than_ask() {
    let (_d, mut app) = app();
    app.settings.set("ai.level", "auto");
    assert_eq!(level(&app), "auto", "no question runs");
    app.assist_route.hand.insert("c1".into(), false);
    assert_eq!(level(&app), "ask");
    assert!(matches!(judged(&app, "program-start foot"), Verdict::Ask(_)));
    app.assist_route.hand.insert("c2".into(), true);
    assert_eq!(level(&app), "auto", "a question of Georg's own hand runs too");
    app.settings.set("ai.level", "answer");
    app.assist_route.hand.clear();
    assert_eq!(level(&app), "answer", "the cap never loosens");
}

