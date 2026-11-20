//! PK-7a: the "Programme" page's model as a state machine (answers and progress lines in, card states and the
//! page's items out), its texts, and the layout of its items.

use super::*;
use crate::layout::Rect;
use crate::page::Part;
use crate::programme_text::{self as text, Tone};
use crate::programme_view::{self as view, Item, Lib, ProgPart, Tab, View};

fn j(s: &str) -> Json {
    Json::parse(s).unwrap()
}

fn t0() -> Instant {
    Instant::now()
}

/// The answer to the last request `what` (by kind).
fn id_of(m: &Model, f: impl Fn(&What) -> bool) -> u64 {
    m.pending.iter().rev().find(|p| f(&p.what)).map(|p| p.id).expect("a pending request of that kind")
}

const PLAN: &str = r#"{"ok":true,"action":"install","name":"foot","plan_id":"p1","installable":true,"program":"foot","version":"1.21.0-1",
  "summary":"foot 1.21.0-1, 3 Teile, 4,0 MB (Download 1,5 MB)","download":1500000,"installed":4000000,
  "packages":[{"repo":"extra","name":"foot","version":"1.21.0-1"},{"repo":"extra","name":"fcft","version":"3.3.1-1"},{"repo":"extra","name":"pixman","version":"0.46.0-1"}],
  "optional":[{"dep":"foot-terminfo","reason":"terminfo für SSH"}],"permissions":["Ordner /data/home/1000/Downloads lesen und schreiben"],
  "refused":[],"space":{"free":50000000000,"total":100000000000,"limit":50000000000,"used":3000000000,"fits":true}}"#;

#[test]
fn install_runs_from_plan_through_progress_to_done() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let jobs = m.refresh(now);
    assert_eq!(jobs.len(), 3);
    assert!(jobs[1].line.contains(r#""op":"list""#) && !jobs[1].stream && jobs[1].wait == READ_WAIT);
    assert!(jobs[2].line.contains(r#""op":"sources""#));
    // An empty list: the first-start offer, no update checks.
    let more = m.on(Msg::End(jobs[1].id, j(r#"{"ok":true,"programs":[]}"#)), now);
    assert!(more.is_empty() && m.listed && m.installed.is_empty());
    let lib = Lib { index: &crate::library::Index::builtin(), rows: &[] };
    let items = view::items(&m, &View { tab: Tab::Add, ..View::default() }, &lib);
    assert!(items.contains(&Item::Head("VORSCHLÄGE".into())), "{items:?}");
    assert!(items.iter().any(|i| matches!(i, Item::Row { title, buttons, .. } if title == "Brave" && buttons[0].0 == ProgPart::Suggest(0))));
    // CR-BR9-1: with Brave installed only LibreOffice's offer stays, with its own index.
    let mut b = m.clone();
    b.installed.push(crate::programme::Installed { name: "brave-bin".into(), ..Default::default() });
    let items = view::items(&b, &View { tab: Tab::Add, ..View::default() }, &lib);
    assert!(!items.iter().any(|i| matches!(i, Item::Row { title, .. } if title == "Brave")), "{items:?}");
    assert!(items.iter().any(|i| matches!(i, Item::Row { title, buttons, .. } if title == "LibreOffice" && buttons[0].0 == ProgPart::Suggest(1))));
    b.installed.push(crate::programme::Installed { name: "libreoffice-fresh".into(), ..Default::default() });
    let items = view::items(&b, &View { tab: Tab::Add, ..View::default() }, &lib);
    assert!(!items.contains(&Item::Head("VORSCHLÄGE".into())), "{items:?}");

    let job = m.plan(Action::Install, "foot", &[], false, now);
    assert!(job.line.contains(r#""op":"plan""#) && job.line.contains(r#""action":"install""#));
    assert_eq!(m.card.as_ref().unwrap().phase, Phase::Asking);
    assert!(m.go(now).is_none(), "nothing runs before the plan is there");
    m.on(Msg::End(job.id, j(PLAN)), now);
    let c = m.card.clone().unwrap();
    assert_eq!((c.phase, c.packages.len(), c.extras.len(), c.plan_id.as_deref()), (Phase::Ready, 3, 1, Some("p1")));
    let lines = text::card_lines(&c);
    assert!(lines.iter().any(|(l, _)| l == "Kommt mit: 3 Teile, 4,0 MB auf der Platte, Download 1,5 MB"), "{lines:?}");
    assert!(lines.iter().any(|(l, t)| l.starts_with("Platz: frei 50 GB") && *t == Tone::Muted), "{lines:?}");

    let run = m.go(now).unwrap();
    assert!(run.stream && run.line == r#"{"op":"do","plan_id":"p1"}"# && run.idle == DO_IDLE);
    assert_eq!(m.card.as_ref().unwrap().phase, Phase::Running);
    for l in [
        r#"{"progress":"download","event":"pkg.download.done","done_bytes":500000,"total_bytes":1500000}"#,
        r#"{"progress":"download","step":"done"}"#,
        r#"{"progress":"verify","step":"done"}"#,
        r#"{"progress":"report","text":"Signatur gut: extra/foot"}"#,
        r#"{"progress":"download","event":"pkg.download.done","done_bytes":1500000,"total_bytes":1500000}"#,
        r#"{"progress":"unpack","step":"done"}"#,
        r#"{"progress":"switch","step":"start"}"#,
    ] {
        assert!(m.on(Msg::Line(run.id, j(l)), now).is_empty());
    }
    let c = m.card.clone().unwrap();
    assert_eq!((c.rank, c.step_done), (Some(4), false), "a late download line never goes back");
    assert_eq!(c.line, "Umschalten …");
    assert_eq!(c.report, ["Signatur gut: extra/foot"]);
    let after = m.on(Msg::End(run.id, j(r#"{"ok":true,"done":true,"action":"install","program":"foot","version":"1.21.0-1","report":["foot installiert"]}"#)), now);
    let c = m.card.clone().unwrap();
    assert_eq!(c.phase, Phase::Done);
    assert_eq!(c.line, "foot 1.21.0-1 ist installiert. Starten in der Bibliothek.");
    // After a run: status, list and the program's details again.
    let ops: Vec<&str> = after.iter().map(|j| j.line.as_str()).collect();
    assert!(ops.iter().any(|l| l.contains("\"list\"")) && ops.iter().any(|l| l.contains("\"space\"")), "{ops:?}");
    assert!(after.iter().any(|j| j.local == Some(PathBuf::from("/apps/foot"))), "the folder's facts");
    assert!(m.events.iter().any(|e| e.contains("install foot: done")), "{:?}", m.events);
}

#[test]
fn refusals_name_their_reason_and_nothing_runs() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let job = m.plan(Action::Install, "libreoffice-fresh", &[], false, now);
    let full = r#"{"ok":true,"installable":false,"summary":"libreoffice-fresh: nicht installierbar","packages":[{"repo":"extra","name":"libreoffice-fresh","version":"25.8"}],
      "download":400000000,"installed":1200000000,"refused":[{"name":"Platz","reason":"zu wenig Platz"}],"space":{"free":800000000,"limit":1000000000,"used":500000000,"fits":false}}"#;
    m.on(Msg::End(job.id, j(full)), now);
    let c = m.card.clone().unwrap();
    assert_eq!(c.phase, Phase::Refused);
    assert_eq!(c.line, "Platz: zu wenig. libreoffice-fresh braucht 1,2 GB (Download 400 MB); frei sind 800 MB, für Programme höchstens 1,0 GB (belegt 500 MB). Nichts wurde heruntergeladen.");
    assert!(m.go(now).is_none());
    // A run that bitspkgd refuses: the German reason with its kind.
    assert_eq!(text::reason("refused", "pkg.pgp.refused: Schlüssel 0123 fehlt"), "Signatur: pkg.pgp.refused: Schlüssel 0123 fehlt");
    // English words of the system or a library: a fixed German sentence with its kind.
    assert_eq!(text::reason("refused", "pkg.pgp.refused: unknown key"), "Signatur: der Schlüssel ist unbekannt oder abgelaufen");
    assert_eq!(text::reason("failed", "write /data/apps/x: No space left on device (os error 28)"), "Platz: die Platte ist voll");
    assert_eq!(text::reason("failed", "connect: Connection refused (os error 111)"), "Spiegel: keine Verbindung zum Spiegel (Netz?)");
    assert_eq!(text::reason("failed", "sha256 mismatch for fcft"), "Signatur: die Prüfsumme stimmt nicht: das Paket ist beschädigt");
    assert_eq!(text::reason("failed", "bad signature: untrusted key"), "Signatur: die Signatur stimmt nicht");
    assert_eq!(text::reason("refused", "kein Spiegel antwortete"), "Spiegel: kein Spiegel antwortete");
    assert_eq!(text::reason("busy", "arbeitet gerade"), "die Programmverwaltung arbeitet gerade an etwas anderem; gleich noch einmal");
    let job = m.plan(Action::Install, "foot", &[], false, now);
    m.on(Msg::End(job.id, j(PLAN)), now);
    let run = m.go(now).unwrap();
    m.on(Msg::End(run.id, j(r#"{"ok":false,"error":"refused","message":"Download: kein Spiegel hatte fcft-3.3.1-1","report":["Spiegel 1: 404"]}"#)), now);
    let c = m.card.clone().unwrap();
    assert_eq!(c.phase, Phase::Failed);
    assert_eq!(c.line, "foot nicht installiert: Spiegel: Download: kein Spiegel hatte fcft-3.3.1-1");
    // Ticked extras the service does not take back: unticked, nothing to run.
    let job = m.plan(Action::Install, "foot", &["foot-terminfo".into()], false, now);
    m.on(Msg::End(job.id, j(PLAN)), now);
    let c = m.card.clone().unwrap();
    assert_eq!(c.phase, Phase::Refused);
    assert!(c.extras.iter().all(|e| !e.2) && c.line.starts_with("Zusätze"));
    let job = m.plan(Action::Install, "foot", &["foot-terminfo".into()], false, now);
    assert!(job.line.contains(r#""with":["foot-terminfo"]"#));
    m.on(Msg::End(job.id, j(&PLAN.replace(r#""refused":[]"#, r#""refused":[],"with":["foot-terminfo"]"#))), now);
    assert_eq!(m.card.as_ref().unwrap().phase, Phase::Ready);
    assert!(m.card.as_ref().unwrap().extras[0].2);
}

#[test]
fn a_missing_or_slow_service_says_it_does_not_answer() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let jobs = m.refresh(now);
    m.on(Msg::Failed(jobs[0].id, "Programmverwaltung nicht erreichbar (/run/bitsos/pkg.sock: No such file)".into()), now);
    assert_eq!(m.live, Some(false));
    // A slow one: the thread's Late, or the main loop's deadline (only once it is due).
    let s = m.search("ripgrep", now).unwrap();
    assert!(m.expire(s.id, now).is_empty() && m.pending.iter().any(|p| p.id == s.id), "not due yet");
    m.expire(s.id, now + READ_WAIT + Duration::from_millis(1));
    assert!(m.pending.iter().all(|p| p.id != s.id));
    assert_eq!(m.search_line, "Programmverwaltung antwortet nicht (nach 10 s)");
    assert_eq!(m.down, LATE);
    let lib = Lib { index: &crate::library::Index::builtin(), rows: &[] };
    let items = view::items(&m, &View::default(), &lib);
    assert!(matches!(&items[1], Item::Line(t, Tone::Bad) if t.starts_with("Programmverwaltung antwortet nicht")), "{items:?}");
    // A late plan and a late run fail their card.
    let p = m.plan(Action::Install, "foot", &[], false, now);
    m.on(Msg::Late(p.id), now);
    assert_eq!(m.card.as_ref().unwrap().phase, Phase::Failed);
    // A late answer after the deadline is dropped (its request is gone).
    assert!(m.on(Msg::End(p.id, j(PLAN)), now).is_empty());
    assert_eq!(m.card.as_ref().unwrap().phase, Phase::Failed);
    // An old search's answer does not replace the newer one's.
    let a = m.search("rip", now).unwrap();
    let b = m.search("ripgrep", now).unwrap();
    m.on(Msg::End(b.id, j(r#"{"ok":true,"hits":[{"repo":"extra","name":"ripgrep","version":"14.1","desc":"grep","rust":true,"isize":5000000}],"more":0}"#)), now);
    m.on(Msg::End(a.id, j(r#"{"ok":true,"hits":[],"more":0}"#)), now);
    assert_eq!(m.hits.len(), 1);
    assert!(m.hits[0].rust && m.hits[0].size == Some(5_000_000) && m.live == Some(true));
}

#[test]
fn removal_keeps_data_only_where_the_service_says_so_and_updates_run_in_order() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let r = m.plan(Action::Remove, "foot", &[], true, now);
    assert!(r.line.contains(r#""keep_data":true"#));
    let plan = r#"{"ok":true,"action":"remove","installable":true,"plan_id":"r1","summary":"foot e1_1.21+1","data":2000,"data_removed":true}"#;
    m.on(Msg::End(r.id, j(plan)), now);
    assert_eq!(m.card.as_ref().unwrap().phase, Phase::Refused, "keep_data not echoed: nothing removed");
    let r = m.plan(Action::Remove, "foot", &[], true, now);
    m.on(Msg::End(r.id, j(&plan.replace("\"data\":2000", "\"keep_data\":true,\"data\":2000"))), now);
    let c = m.card.clone().unwrap();
    assert_eq!(c.phase, Phase::Ready);
    assert!(text::card_lines(&c).iter().any(|(l, _)| l == "Deine Daten bleiben (Meine Daten behalten)."));
    let go = m.go(now).unwrap();
    m.details.insert("foot".into(), Detail::default());
    m.on(Msg::End(go.id, j(r#"{"ok":true,"done":true,"action":"remove","freed":3000000,"report":[]}"#)), now);
    assert_eq!(m.card.as_ref().unwrap().line, "foot ist entfernt (3,0 MB frei).");
    assert!(!m.details.contains_key("foot"));

    // The update list from one plan per installed program; "Alle aktualisieren" runs them one after another.
    // (the removal's end asked the list already: a refresh now asks nothing twice)
    assert!(m.refresh(now).is_empty());
    let l = id_of(&m, |w| matches!(w, What::List(_)));
    let checks = m.on(Msg::End(l, j(r#"{"ok":true,"programs":[{"name":"jq","current":"a","previous":null,"versions":["a"]},{"name":"foot","current":"b","previous":"a","versions":["a","b"]}]}"#)), now);
    assert_eq!(checks.len(), 2);
    for c in &checks {
        let name = if c.line.contains("\"jq\"") { "jq" } else { "foot" };
        let reply = format!(r#"{{"ok":true,"installable":true,"plan_id":"u-{name}","version":"2","summary":"{name} 2, 1 Teile geändert","changed":[{{"name":"x","from":"1","to":"2"}}]}}"#);
        m.on(Msg::End(c.id, j(&reply)), now);
    }
    assert_eq!(m.updates.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(), ["foot", "jq"]);
    let names: Vec<String> = m.updates.iter().map(|u| u.name.clone()).collect();
    let first = m.update(&names, now).unwrap();
    assert_eq!(first.line, r#"{"op":"do","plan_id":"u-foot"}"#);
    let next = m.on(Msg::End(first.id, j(r#"{"ok":true,"done":true,"report":[]}"#)), now);
    assert!(next.iter().any(|j| j.line == r#"{"op":"do","plan_id":"u-jq"}"#), "{next:?}");
    assert_eq!(m.card.as_ref().unwrap().name, "jq");
    // A rollback answers in the page's line and asks again.
    let rb = m.rollback("foot", now);
    let again = m.on(Msg::End(rb.id, j(r#"{"ok":true,"from":"b","to":"a","report":["3 Teile älter"]}"#)), now);
    assert_eq!(m.note.clone().unwrap(), ("foot: zurück auf a (vorher b). 3 Teile älter".to_string(), false));
    assert!(!again.is_empty());
    let _ = id_of(&m, |w| matches!(w, What::List(_)));
}

#[test]
fn names_from_an_address_never_become_paths() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    for bad in ["..", ".", "/etc", "a/b", "", "-x", "Foot", "x\0y"] {
        assert!(!valid_name(bad), "{bad:?}");
        assert!(m.detail(bad, now).is_empty(), "{bad:?}");
    }
    assert!(valid_name("foot") && valid_name("libreoffice-fresh") && valid_name("gtk+3") && valid_name("lib32@x_1.2"));
    assert!(m.detail("foot", now).iter().any(|j| j.local == Some(PathBuf::from("/apps/foot"))));
    let lib = Lib { index: &crate::library::Index::builtin(), rows: &[] };
    let items = view::items(&m, &View::of_address("programme/.."), &lib);
    assert!(items.iter().any(|i| matches!(i, Item::Line(t, Tone::Bad) if t.starts_with("Unbekanntes Programm"))), "{items:?}");
}

#[test]
fn requests_are_not_asked_twice_and_stale_answers_are_dropped() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let first = m.refresh(now);
    assert!(m.refresh(now).is_empty(), "a tab change while all three are asked asks nothing");
    let list = r#"{"ok":true,"programs":[{"name":"jq","current":"a","previous":null,"versions":["a"]}]}"#;
    let checks = m.on(Msg::End(first[1].id, j(list)), now);
    assert_eq!(checks.len(), 1);
    // Another list while jq's check is out: no second check for jq.
    let again = m.refresh(now);
    let l = again.iter().find(|j| j.line.contains("\"list\"")).unwrap();
    assert!(m.on(Msg::End(l.id, j(list)), now).is_empty());
    // A run ends: the generation moves on, the old check's answer is stale.
    let p = m.plan(Action::Install, "foot", &[], false, now);
    m.on(Msg::End(p.id, j(PLAN)), now);
    let run = m.go(now).unwrap();
    let after = m.on(Msg::End(run.id, j(r#"{"ok":true,"done":true,"report":[]}"#)), now);
    assert!(after.iter().any(|j| j.line.contains("\"list\"")), "after a run the list is asked anew");
    let upd = r#"{"ok":true,"installable":true,"plan_id":"u","version":"2","summary":"jq 2","changed":[{"name":"jq"}]}"#;
    m.on(Msg::End(checks[0].id, j(upd)), now);
    assert!(m.updates.is_empty(), "a check asked before the run does not count");
}

#[test]
fn an_incomplete_check_or_missing_sources_never_say_all_is_current() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let lib = Lib { index: &crate::library::Index::builtin(), rows: &[] };
    let lines = |m: &Model, tab| -> Vec<String> { view::items(m, &View { tab, ..View::default() }, &lib).into_iter().filter_map(|i| if let Item::Line(t, _) = i { Some(t) } else { None }).collect() };
    let r = m.refresh(now);
    m.on(Msg::End(r[2].id, j(r#"{"ok":true,"sources":[{"name":"core","packages":200,"age_days":0},{"name":"extra","packages":null,"age_days":null}]}"#)), now);
    assert!(m.sources_line.as_deref().is_some_and(|l| l.starts_with("Paketquellen noch nicht geladen: extra.")), "{:?}", m.sources_line);
    let checks = m.on(Msg::End(r[1].id, j(r#"{"ok":true,"programs":[{"name":"jq","current":"a"},{"name":"foot","current":"b"}]}"#)), now);
    assert!(lines(&m, Tab::Updates).iter().any(|l| l == "Wird geprüft …"));
    m.on(Msg::End(checks[0].id, j(r#"{"ok":false,"error":"refused","message":"kein Spiegel antwortete"}"#)), now);
    m.on(Msg::Late(checks[1].id), now);
    let u = lines(&m, Tab::Updates);
    assert!(!u.iter().any(|l| l == "Alles ist aktuell."), "{u:?}");
    assert!(u.iter().any(|l| l == "Prüfung unvollständig: jq: Spiegel: kein Spiegel antwortete"), "{u:?}");
    assert!(u.iter().any(|l| l.starts_with("Prüfung unvollständig: foot: Programmverwaltung antwortet nicht")), "{u:?}");
    // A search without hits says why (the sources), and names what it did not search.
    let s = m.search("rip", now).unwrap();
    m.on(Msg::End(s.id, j(r#"{"ok":true,"hits":[],"more":0,"notes":["extra"]}"#)), now);
    assert!(m.search_line.starts_with("Paketquellen noch nicht geladen") && m.search_note == "Nicht durchsucht, weil noch nicht geladen: extra.");
    // Sources fine, every check in: now it may say so.
    let fresh = r#"{"ok":true,"sources":[{"name":"core","packages":200,"age_days":1}]}"#;
    assert_eq!(text::sources_line(j(fresh).get("sources").and_then(Json::as_array).unwrap()), None);
    assert!(text::sources_line(j(r#"{"s":[{"name":"core","packages":2,"age_days":30}]}"#).get("s").and_then(Json::as_array).unwrap()).is_some_and(|l| l.contains("vor 30 Tagen")));
    let r = m.refresh(now);
    let src = r.iter().find(|j| j.line.contains("sources")).unwrap();
    m.on(Msg::End(src.id, j(fresh)), now);
    let l = r.iter().find(|j| j.line.contains("\"list\"")).unwrap();
    for c in m.on(Msg::End(l.id, j(r#"{"ok":true,"programs":[{"name":"jq","current":"a"}]}"#)), now) {
        m.on(Msg::End(c.id, j(r#"{"ok":true,"installable":true,"changed":[]}"#)), now);
    }
    assert!(lines(&m, Tab::Updates).iter().any(|l| l == "Alles ist aktuell."), "{:?}", lines(&m, Tab::Updates));
}

#[test]
fn check_now_loads_the_sources_and_asks_again() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let job = m.check_now(now).unwrap();
    assert!(job.stream && job.wait == REFRESH_WAIT && job.line == r#"{"op":"refresh"}"#);
    assert!(m.check_now(now).is_none() && m.refreshing());
    assert!(m.on(Msg::Line(job.id, j(r#"{"progress":"download","step":"start"}"#)), now).is_empty());
    let again = m.on(Msg::End(job.id, j(r#"{"ok":true,"done":true,"refused":0,"report":["core: 200 Pakete"]}"#)), now);
    assert!(again.iter().any(|j| j.line.contains("\"list\"")) && m.checked_at.is_some());
    assert_eq!(m.note.clone().unwrap(), ("Paketquellen geladen.".to_string(), false));
    let job = m.check_now(now).unwrap();
    m.on(Msg::End(job.id, j(r#"{"ok":false,"error":"refused","message":"kein Spiegel antwortete"}"#)), now);
    assert_eq!(m.note.clone().unwrap(), ("Prüfung fehlgeschlagen: Spiegel: kein Spiegel antwortete".to_string(), true));
}

#[test]
fn what_bitspkgd_cannot_yet_is_greyed_not_refused_after_a_click() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let lib = Lib { index: &crate::library::Index::builtin(), rows: &[] };
    let r = m.refresh(now);
    m.on(Msg::End(r[1].id, j(r#"{"ok":true,"programs":[{"name":"foot","current":"b"}]}"#)), now);
    m.on(Msg::End(r[0].id, j(r#"{"ok":true,"last_check":1}"#)), now);
    assert_eq!(m.features, Some(vec![]));
    let v = View { tab: Tab::Program, program: Some("foot".into()), keep_data: false };
    let items = view::items(&m, &v, &lib);
    assert!(!items.iter().any(|i| matches!(i, Item::Check(ProgPart::KeepData, ..))), "{items:?}");
    assert!(items.iter().any(|i| matches!(i, Item::Buttons(b) if b.contains(&(ProgPart::Clean, "Cache leeren".into(), false)))));
    assert!(items.iter().any(|i| matches!(i, Item::Line(t, _) if t == text::NO_CLEAN)));
    let p = m.plan(Action::Install, "foot", &[], false, now);
    m.on(Msg::End(p.id, j(PLAN)), now);
    let add = view::items(&m, &View { tab: Tab::Add, ..View::default() }, &lib);
    assert!(!add.iter().any(|i| matches!(i, Item::Check(ProgPart::Extra(_), ..))) && add.iter().any(|i| matches!(i, Item::Line(t, Tone::Item) if t.starts_with("foot-terminfo"))));
    // Once bitspkgd names them: the box, the button.
    let r = m.refresh(now);
    m.on(Msg::End(r[0].id, j(r#"{"ok":true,"features":["keep_data","with","clean"]}"#)), now);
    let items = view::items(&m, &v, &lib);
    assert!(items.iter().any(|i| matches!(i, Item::Check(ProgPart::KeepData, ..))));
    assert!(items.iter().any(|i| matches!(i, Item::Buttons(b) if b.contains(&(ProgPart::Clean, "Cache leeren".into(), true)))));
    let add = view::items(&m, &View { tab: Tab::Add, ..View::default() }, &lib);
    assert!(add.iter().any(|i| matches!(i, Item::Check(ProgPart::Extra(0), ..))));
}

#[test]
fn the_items_lay_out_without_overlap_and_hit_their_parts() {
    let now = t0();
    let mut m = Model::new("/apps".into());
    let job = m.plan(Action::Install, "foot", &[], false, now);
    m.on(Msg::End(job.id, j(PLAN)), now);
    let s = m.search("foot", now).unwrap();
    m.on(Msg::End(s.id, j(r#"{"ok":true,"hits":[{"repo":"extra","name":"foot","version":"1.21","desc":"Wayland terminal","rust":false}],"more":3}"#)), now);
    let ix = crate::library::Index::builtin();
    let rows = crate::library_view::rows(&ix, &[], &[], "");
    let lib = Lib { index: &ix, rows: &rows };
    let tabs: Vec<String> = view::TABS.iter().map(|t| t.1.to_string()).collect();
    for tab in [Tab::Library, Tab::Add, Tab::Updates] {
        let items = view::items(&m, &View { tab, ..View::default() }, &lib);
        for c in [Rect { x: 0, y: 0, w: 1600, h: 4000 }, Rect { x: 40, y: 30, w: 420, h: 4000 }] {
            let l = crate::programme_layout::layout(c, &items, &tabs, 0);
            assert_eq!(l.items.len(), items.len());
            for (i, (a, ra)) in l.parts.iter().enumerate() {
                assert!(ra.w > 0 && ra.h > 0, "{a:?}");
                for (b, rb) in &l.parts[i + 1..] {
                    let overlap = ra.x < rb.x + rb.w && rb.x < ra.x + ra.w && ra.y < rb.y + rb.h && rb.y < ra.y + ra.h;
                    // A package tile's "…" lies on its tile (it comes first, so it is hit first).
                    let nested = matches!((a, b), (Part::Prog(ProgPart::More(x)), Part::Prog(ProgPart::Tile(y))) if x == y);
                    assert!(!overlap || nested, "{tab:?} {c:?}: {a:?} and {b:?}");
                }
                let (cx, cy) = (ra.x as f64 + ra.w as f64 / 2.0, ra.y as f64 + ra.h as f64 / 2.0);
                assert_eq!(l.hit(cx, cy), Some(*a));
            }
        }
    }
    let items = view::items(&m, &View { tab: Tab::Add, ..View::default() }, &lib);
    let l = crate::programme_layout::layout(Rect { x: 0, y: 0, w: 1600, h: 900 }, &items, &tabs, 0);
    assert!(l.part(ProgPart::CardGo).is_some() && l.part(ProgPart::Install(0)).is_some() && l.part(ProgPart::Search).is_some());
    assert_eq!(view::part_name(&m, &items, ProgPart::Install(0)), "install=foot");
    // A 2560 px screen's window: the page uses up to 1440 of it (it stopped at 1000 and left the rest empty).
    let wide = crate::programme_layout::layout(Rect { x: 0, y: 0, w: 2200, h: 1200 }, &items, &tabs, 0);
    assert_eq!(wide.part(ProgPart::Search).map(|r| r.w), Some(1440));
    // Scrolled down: what went under the head can no longer be hit, the tabs still can.
    let l = crate::programme_layout::layout(Rect { x: 0, y: 0, w: 1600, h: 900 }, &items, &tabs, 200);
    assert!(l.part(ProgPart::Search).is_none() && l.part(ProgPart::Tab(1)).is_some());
    // Every text of the page has its glyphs.
    for it in &items {
        if let Item::Line(t, _) | Item::Head(t) = it {
            assert!(t.chars().all(crate::text::Font::has), "{t}");
        }
    }
    for w in [crate::programme_draw::PLACEHOLDER, crate::programme_app::REAL_INPUT] {
        assert!(w.chars().all(crate::text::Font::has), "{w}");
    }
}

#[test]
fn addresses_open_their_tab_and_bytes_are_german() {
    assert_eq!(View::of_address("programme/").tab, Tab::Library);
    assert_eq!(View::of_address("programme/hinzufuegen").tab, Tab::Add);
    assert_eq!(View::of_address("programme/aktualisierungen").address(), "programme/aktualisierungen");
    let v = View::of_address("programme/foot");
    assert_eq!((v.tab, v.program.as_deref(), v.address().as_str()), (Tab::Program, Some("foot"), "programme/foot"));
    assert_eq!(View::default().address(), "programme/");
    assert_eq!(text::bytes(0), "0 Bytes");
    assert_eq!(text::bytes(1_500), "1,5 KB");
    assert_eq!(text::bytes(340_000_000), "340 MB");
    assert_eq!(text::bytes(12_300_000_000), "12 GB");
}
