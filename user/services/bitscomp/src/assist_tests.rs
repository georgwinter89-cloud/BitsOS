//! CL-3b: the conversation's state machine: lines of the socket in, the ring, the list and the request lines out.

use super::*;
use crate::desk::Ring;

const S: fn(u64) -> Duration = Duration::from_secs;

fn j(s: &str) -> Json {
    Json::parse(s).unwrap()
}

/// A connected model whose assistant said it is ready, showing chat c1 (EINS-B: protocol v2, PLAN K1).
fn ready() -> Model {
    let mut m = Model::new("Claude");
    let asked = m.link_up(S(0));
    assert_eq!(asked, [r#"{"op":"status"}"#, r#"{"op":"chats"}"#], "no chat shown yet: only the list");
    m.on(&j(r#"{"ok":true,"name":"Mira","provider":"anthropic","place":"cloud","local_only":false,"ready":true,"busy":false,"turns":0}"#), S(0));
    m.on(&j(r#"{"ok":true,"chats":[]}"#), S(0));
    assert_eq!((m.gate(), m.pending(), m.name.as_str()), (Gate::Ready, 0, "Mira"));
    m.chat = Some("c1".into());
    m
}

fn ask(m: &mut Model, text: &str, at: u64) -> u64 {
    m.edit.text = text.into();
    let out = m.submit(S(at), false);
    assert_eq!(out.len(), 1, "{:?}", m.note);
    m.ask_id().unwrap()
}

fn ev(m: &mut Model, line: &str, at: u64) -> Vec<String> {
    m.on(&j(line), S(at))
}

#[test]
fn a_streamed_answer_turns_the_ring_and_grows_one_item() {
    let mut m = ready();
    let id = ask(&mut m, "  Wie spät ist es?  ", 1);
    assert_eq!((m.phase, m.phase.ring(), m.edit.text.as_str()), (Phase::Thinking, Ring::Thinking, ""));
    assert_eq!(m.items, [Item::Turn { who: Who::Georg, text: "Wie spät ist es?".into() }]);
    ev(&mut m, &format!(r#"{{"ev":"start","id":{id},"provider":"anthropic","place":"cloud"}}"#), 1);
    assert_eq!(m.phase, Phase::Thinking);
    ev(&mut m, &format!(r#"{{"ev":"notice","id":{id},"text":"geht an Anthropic"}}"#), 1);
    assert_eq!(m.notice.as_deref(), Some("geht an Anthropic"));
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"Es ist "}}"#), 2);
    assert_eq!((m.phase, m.phase.ring(), m.phase.word()), (Phase::Answering, Ring::Working, "ANTWORTET"));
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"kurz nach drei."}}"#), 2);
    assert_eq!(m.items.len(), 2);
    assert_eq!(m.items[1], Item::Turn { who: Who::Assistant, text: "Es ist kurz nach drei.".into() });
    // The growing item is the one the drawing wraps again.
    assert_eq!(m.stale_from, 0);
    m.stale_from = usize::MAX;
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":" Gern."}}"#), 2);
    assert_eq!(m.stale_from, 1);
    // A status that comes while the own answer runs says busy: that is no foreign answer to read again later.
    assert!(m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":true,"turns":1}"#), S(2)).is_empty());
    let after = ev(&mut m, &format!(r#"{{"ev":"done","id":{id}}}"#), 3);
    assert_eq!(after, [r#"{"op":"status"}"#], "the turn count is asked again");
    assert!(m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":2}"#), S(3)).is_empty(), "the list is not replaced by the history");
    assert_eq!((m.phase, m.ask_id(), m.notice.clone()), (Phase::Idle, None, None));
}

#[test]
fn a_tool_runs_then_text_after_it_is_a_new_paragraph() {
    let mut m = ready();
    let id = ask(&mut m, "Was liegt in Dokumente?", 1);
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"Ich sehe nach."}}"#), 1);
    ev(&mut m, &format!(r#"{{"ev":"tool","id":{id},"name":"list_dir","state":"runs","text":"Liest den Ordner Dokumente"}}"#), 2);
    assert_eq!((m.phase, m.phase.ring(), m.tool.as_deref()), (Phase::Working, Ring::Working, Some("Liest den Ordner Dokumente")));
    ev(&mut m, &format!(r#"{{"ev":"tool","id":{id},"name":"list_dir","state":"done","text":""}}"#), 3);
    assert_eq!((m.phase, m.tool.clone()), (Phase::Thinking, None));
    assert_eq!(m.items[2], Item::Tool { name: "list_dir".into(), text: "Liest den Ordner Dokumente".into(), state: ToolState::Done }, "a closing line without words keeps the tool's own");
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"Drei Dateien."}}"#), 4);
    assert_eq!(m.items.len(), 4);
    assert_eq!(m.items[3], Item::Turn { who: Who::Assistant, text: "Drei Dateien.".into() });
}

#[test]
fn cancel_ends_with_the_assistants_word_and_late_lines_are_dropped() {
    let mut m = ready();
    let id = ask(&mut m, "Erzähl lange.", 1);
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"Es war einmal"}}"#), 1);
    ev(&mut m, &format!(r#"{{"ev":"tool","id":{id},"name":"read_file","state":"runs","text":"Liest a.txt"}}"#), 1);
    assert_eq!(m.cancel(S(1)), [format!(r#"{{"op":"cancel","chat":"c1","id":{id}}}"#)]);
    assert_eq!(m.phase, Phase::Working, "the assistant says when it stopped");
    ev(&mut m, &format!(r#"{{"ev":"error","id":{id},"message":"abgebrochen"}}"#), 2);
    assert_eq!((m.phase, m.ask_id()), (Phase::Idle, None));
    assert_eq!(m.items.last(), Some(&Item::Note { text: "abgebrochen".into(), error: false }));
    assert!(matches!(&m.items[2], Item::Tool { state: ToolState::Stopped, .. }), "the open tool did not finish");
    let n = m.items.len();
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":" zu spät"}}"#), 3);
    assert_eq!(m.items.len(), n);
    assert!(m.cancel(S(3)).is_empty());
    // A real error is one.
    let id = ask(&mut m, "Noch einmal.", 4);
    ev(&mut m, &format!(r#"{{"ev":"error","id":{id},"message":"Der Anbieter antwortet nicht."}}"#), 5);
    assert_eq!(m.items.last(), Some(&Item::Note { text: "Der Anbieter antwortet nicht.".into(), error: true }));
}

#[test]
fn a_cancel_the_assistant_does_not_confirm_ends_here_after_its_deadline() {
    // The provider hangs: the assistant's worker never says "abgebrochen".
    let mut m = ready();
    let id = ask(&mut m, "Hängst du?", 1);
    ev(&mut m, &format!(r#"{{"ev":"start","id":{id}}}"#), 1);
    assert_eq!(m.cancel(S(5)).len(), 1);
    assert_eq!(m.due(), Some(S(5) + STATUS_WAIT), "the cancel has a deadline of its own");
    // A second Esc sends the cancel again and keeps the first deadline.
    assert_eq!(m.cancel(S(6)).len(), 1);
    assert_eq!(m.due(), Some(S(5) + STATUS_WAIT));
    assert!(m.tick(S(5) + STATUS_WAIT - Duration::from_millis(1)).is_empty());
    assert_eq!(m.phase, Phase::Thinking);
    // Over for Georg; the status is asked, so a helper that is still busy shows as busy.
    assert_eq!(m.tick(S(5) + STATUS_WAIT), [r#"{"op":"status"}"#]);
    assert_eq!((m.phase, m.ask_id()), (Phase::Idle, None));
    assert_eq!(m.items.last(), Some(&Item::Note { text: "abgebrochen".into(), error: false }), "not an error: Georg asked for it");
    // The assistant's own word, when it comes after all, is a late line and changes nothing.
    let n = m.items.len();
    ev(&mut m, &format!(r#"{{"ev":"error","id":{id},"message":"abgebrochen"}}"#), 30);
    assert_eq!(m.items.len(), n);
    // Also while a question card is open (waiting has no deadline, the cancel has).
    m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":1}"#), S(31));
    let id = ask(&mut m, "Lösch es.", 40);
    ev(&mut m, &format!(r#"{{"ev":"tool","id":{id},"name":"delete_file","state":"asks","text":"Löscht a.txt","ask_id":7}}"#), 41);
    m.tick(S(41) + ARM);
    assert_eq!(m.due(), None);
    m.cancel(S(50));
    assert_eq!(m.due(), Some(S(50) + STATUS_WAIT));
    m.tick(S(50) + STATUS_WAIT);
    assert_eq!((m.card.clone(), m.phase), (None, Phase::Idle));
}

#[test]
fn not_ready_says_why_and_sends_nothing() {
    let mut m = Model::new("Claude");
    m.link_up(S(0));
    assert_eq!(m.gate(), Gate::Starting);
    m.on(&j(r#"{"ok":true,"name":"Claude","provider":null,"place":null,"local_only":false,"ready":false,"why":"Es ist noch kein Anbieter eingerichtet.","busy":false,"turns":0}"#), S(0));
    assert_eq!(m.gate(), Gate::NotReady("Es ist noch kein Anbieter eingerichtet.".into()));
    m.edit.text = "Hallo".into();
    assert!(m.submit(S(1), false).is_empty());
    assert_eq!(m.note, Some(("Es ist noch kein Anbieter eingerichtet.".into(), true)));
    assert_eq!((m.edit.text.as_str(), m.items.len()), ("Hallo", 0), "the question stays in the input");
    // A provider was added on the settings page: the status is asked again and the question goes.
    assert_eq!(m.refresh(S(2)), [r#"{"op":"status"}"#]);
    m.on(&j(r#"{"ok":true,"name":"Claude","provider":"ollama","place":"local","local_only":true,"ready":true,"busy":false,"turns":0}"#), S(2));
    assert_eq!(m.submit(S(3), false).len(), 1);
}

#[test]
fn settings_not_reachable_yet_are_asked_again_until_they_are() {
    // assist starts before settingsd at boot: its first status says so, and nobody touches the sidebar.
    let down = r#"{"ok":true,"name":"Assistent","provider":null,"place":null,"local_only":false,"ready":false,"why":"Die Einstellungen sind gerade nicht erreichbar.","busy":false,"turns":0}"#;
    let (first, max) = (Duration::from_millis(500), Duration::from_secs(8));
    let mut m = Model::new("Claude");
    m.link_up(S(0));
    m.on(&j(r#"{"ok":true,"turns":[]}"#), S(0));
    m.on(&j(down), S(0));
    assert_eq!(m.due(), Some(first), "asked again without an interaction");
    assert_eq!(m.tick(first), [r#"{"op":"status"}"#]);
    m.on(&j(down), first);
    assert_eq!(m.due(), Some(first * 3), "the wait doubles");
    let mut at = first * 3;
    for _ in 0..8 {
        assert_eq!(m.tick(at), [r#"{"op":"status"}"#]);
        m.on(&j(down), at);
        let next = m.due().expect("asked again");
        assert!(next - at <= max, "at most {max:?}");
        at = next;
    }
    // A newer assist names the reason itself.
    m.on(&j(&down.replace("Die Einstellungen sind gerade nicht erreichbar.", "x").replace(r#""turns":0"#, r#""turns":0,"settings_down":true"#)), at);
    assert!(m.due().is_some());
    // settingsd is up: ready, and nothing is asked any more.
    m.on(&j(r#"{"ok":true,"name":"Claude","provider":"abo","place":"cloud","local_only":false,"ready":true,"busy":false,"turns":0}"#), at);
    assert_eq!((m.gate(), m.due(), m.name.as_str()), (Gate::Ready, None, "Claude"));
    // Not ready for another reason: no polling either.
    m.on(&j(r#"{"ok":true,"name":"Claude","provider":null,"place":null,"local_only":false,"ready":false,"why":"Es ist noch kein Anbieter eingerichtet.","busy":false,"turns":0}"#), at);
    assert_eq!(m.due(), None);
}

#[test]
fn a_silent_assistant_is_named_after_each_deadline() {
    // No socket: said after the grace, not at once (the helper starts with the session).
    let mut m = Model::new("Mira");
    m.link_down("connect: No such file", S(0));
    assert_eq!((m.gate(), m.due()), (Gate::Starting, Some(LINK_GRACE)));
    m.tick(LINK_GRACE);
    assert_eq!((m.gate(), m.silent_line().as_str(), m.due()), (Gate::Silent, "Mira antwortet nicht.", None));
    m.edit.text = "Hallo".into();
    assert!(m.submit(S(9), false).is_empty());
    assert_eq!(m.note, Some(("Mira antwortet nicht.".into(), true)));
    m.chat = Some("c1".into());
    // It comes: everything is asked; no reply to the status within its deadline is silence again, asked again later.
    m.link_up(S(10));
    assert_eq!((m.gate(), m.due()), (Gate::Starting, Some(S(10) + STATUS_WAIT)));
    assert!(m.tick(S(10) + STATUS_WAIT).is_empty());
    assert_eq!(m.gate(), Gate::Silent);
    assert_eq!(m.tick(S(10) + STATUS_WAIT + POLL), [r#"{"op":"status"}"#]);
    m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":0}"#), S(16));
    assert_eq!(m.gate(), Gate::Ready);
    // An ask without its start line, and an answer that stops arriving: both end, cancelled, with the sentence.
    let id = ask(&mut m, "Bist du da?", 20);
    assert_eq!(m.due(), Some(S(20) + ASK_START));
    assert_eq!(m.tick(S(20) + ASK_START), [format!(r#"{{"op":"cancel","chat":"c1","id":{id}}}"#)]);
    assert_eq!((m.phase, m.ask_id()), (Phase::Idle, None));
    assert_eq!(m.items.last(), Some(&Item::Note { text: "Mira antwortet nicht.".into(), error: true }));
    let id = ask(&mut m, "Und jetzt?", 40);
    ev(&mut m, &format!(r#"{{"ev":"start","id":{id}}}"#), 41);
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"Ja"}}"#), 42);
    assert!(m.tick(S(42) + ASK_IDLE - S(1)).is_empty());
    assert_eq!(m.tick(S(42) + ASK_IDLE).len(), 1);
    assert_eq!(m.items.last(), Some(&Item::Note { text: "Mira antwortet nicht.".into(), error: true }));
    // The socket goes while an answer runs.
    let _ = ask(&mut m, "Noch da?", 400);
    m.link_down("the assistant closed the connection", S(401));
    assert_eq!((m.phase, m.ask_id(), m.status.clone()), (Phase::Idle, None, None));
}

#[test]
fn a_restart_reads_the_conversation_and_a_new_one_empties_it() {
    let mut m = Model::new("Claude");
    m.chat = Some("c1".into());
    assert_eq!(m.link_up(S(0)), [r#"{"op":"status"}"#, r#"{"op":"chats"}"#, r#"{"op":"history","chat":"c1"}"#], "the shown chat is read again");
    m.on(&j(r#"{"ok":true,"name":"Claude","ready":true,"busy":false,"turns":2}"#), S(0));
    m.on(&j(r#"{"ok":true,"chat":"c9","turns":[{"who":"georg","text":"Anderes"}]}"#), S(0));
    assert!(m.items.is_empty(), "another chat's history is not this view's");
    m.on(&j(r#"{"ok":true,"chat":"c1","turns":[{"who":"georg","text":"Hallo"},{"who":"assistant","text":"Hallo Georg."}]}"#), S(0));
    assert_eq!(m.items, [Item::Turn { who: Who::Georg, text: "Hallo".into() }, Item::Turn { who: Who::Assistant, text: "Hallo Georg.".into() }]);
    assert_eq!(m.status.as_ref().map(|s| s.turns), Some(2));
    // Busy with a second question: the assistant's refusal ends it in the list, the first one goes on.
    let id = ask(&mut m, "Eins", 1);
    m.edit.text = "Zwei".into();
    assert!(m.submit(S(1), false).is_empty());
    assert_eq!(m.note.as_ref().map(|n| n.0.as_str()), Some("Claude antwortet noch. Esc bricht ab."));
    ev(&mut m, &format!(r#"{{"ev":"start","id":{id}}}"#), 1);
    // "+ Neuer Chat" while it runs: the view is empty, the running answer goes on in its chat (in the list); the
    // new chat is made with its first question.
    let out = m.new_conversation(S(2));
    assert_eq!(out, [r#"{"op":"status"}"#.to_string()]);
    assert!(m.items.is_empty() && m.ask_id().is_none() && m.stale_from == 0 && m.chat.is_none());
    m.edit.text = "Drei".into();
    assert_eq!(m.submit(S(3), false), [r#"{"op":"new","origin":"sidebar"}"#.to_string()]);
    assert_eq!(m.chat_made("c2", S(4)), [r#"{"op":"ask","chat":"c2","id":2,"text":"Drei","by":"hand"}"#.to_string()]);
    // The service is busy with another chat: no ring here (EINS-B).
    m.on(&j(r#"{"ok":true,"name":"Claude","ready":true,"busy":true,"turns":1}"#), S(5));
    assert!(m.ask_id().is_some());
    ev(&mut m, r#"{"ev":"start","id":2}"#, 5);
    // An unknown op's refusal is a note, the connection and the model stay.
    m.on(&j(r#"{"ok":false,"error":"unknown_op","message":"Das kenne ich nicht."}"#), S(6));
    assert_eq!(m.note, Some(("Das kenne ich nicht.".into(), true)));
}

#[test]
fn a_refusal_of_something_else_leaves_an_open_status_its_deadline() {
    let mut m = ready();
    assert_eq!(m.refresh(S(10)), [r#"{"op":"status"}"#]);
    // The assistant refuses some other request (a cancel of an ask that is over) and never answers the status.
    m.on(&j(r#"{"ok":false,"error":"no_such_ask","message":"Da läuft nichts."}"#), S(11));
    assert_eq!((m.pending(), m.due()), (1, Some(S(10) + STATUS_WAIT)), "the status still has its deadline");
    m.tick(S(10) + STATUS_WAIT);
    assert_eq!(m.gate(), Gate::Silent, "an unanswered status is 'antwortet nicht', not a wait without end");
}

#[test]
fn a_stranger_on_the_socket_is_said_at_once_and_gets_nothing() {
    let mut m = ready();
    let _ = ask(&mut m, "Hallo", 1);
    m.link_stranger("pid 77 is not the helper", S(2));
    assert_eq!((m.gate(), m.ask_id(), m.up), (Gate::Stranger, None, false));
    assert_eq!(m.stranger_line(), "Ein anderes Programm antwortet an der Stelle von Mira.");
    m.edit.text = "mein Schlüssel".into();
    assert!(m.submit(S(3), false).is_empty(), "nothing is sent to it");
    assert_eq!(m.note, Some((m.stranger_line(), true)));
    assert!(m.new_conversation(S(3)).is_empty() && m.refresh(S(3)).is_empty());
    // The grace of a missing socket does not turn it into "antwortet nicht".
    m.tick(S(2) + LINK_GRACE);
    assert_eq!(m.gate(), Gate::Stranger);
    // The real helper is back.
    m.link_up(S(20));
    assert_eq!(m.gate(), Gate::Starting);
}

#[test]
fn what_the_other_end_sends_is_bounded() {
    let mut m = ready();
    let id = ask(&mut m, "Schreib viel.", 1);
    // A long answer goes on in further items of at most a piece each (only the last one is wrapped again
    // when a line arrives); a piece ends at a line end where there is one.
    let line = format!("{}\\n", "x".repeat(999));
    let chunk = line.repeat(40);
    for _ in 0..4 {
        assert!(ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"{chunk}"}}"#), 2).is_empty());
    }
    assert!(m.items.len() >= 4 && m.items[1..].iter().all(|i| i.text().len() <= PIECE), "{:?}", m.items.iter().map(|i| i.text().len()).collect::<Vec<_>>());
    assert!(m.items[1..m.items.len() - 1].iter().all(|i| i.text().ends_with('x') && i.text().len() % 1000 == 999), "pieces end at line ends");
    let shown: usize = m.items[1..].iter().map(|i| i.text().len()).sum();
    assert_eq!(shown + m.items.len() - 2, 4 * 40 * 1000, "nothing but the line ends between pieces is lost");
    // A text without any line end is cut at a character.
    assert!(ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"{}"}}"#, "ä".repeat(PIECE)), 2).is_empty());
    assert!(m.items.iter().all(|i| i.text().len() <= PIECE));
    // More than the list holds: the answer ends, cancelled, with a sentence; later pieces are late lines.
    let big = "y".repeat(LIST_MAX / 4);
    let mut out = Vec::new();
    for _ in 0..4 {
        out = ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"{big}"}}"#), 3);
    }
    assert_eq!(out, [format!(r#"{{"op":"cancel","chat":"c1","id":{id}}}"#), r#"{"op":"status"}"#.to_string()]);
    assert_eq!(m.ask_id(), None);
    assert!(matches!(m.items.last(), Some(Item::Note { error: true, text }) if text.starts_with("Die Antwort ist zu lang")));
    let n = m.items.len();
    ev(&mut m, &format!(r#"{{"ev":"delta","id":{id},"text":"spät"}}"#), 3);
    assert_eq!(m.items.len(), n);
    assert!(m.items.iter().map(|i| i.text().len()).sum::<usize>() <= LIST_MAX);
    // A history larger than the list: the newest turns that fit, in pieces.
    let big = "z".repeat(LIST_MAX / 8);
    let turns: Vec<String> = (0..12).map(|i| format!(r#"{{"who":"{}","text":"{i}{big}"}}"#, if i % 2 == 0 { "georg" } else { "assistant" })).collect();
    m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":12}"#), S(4));
    m.on(&j(&format!(r#"{{"ok":true,"turns":[{}]}}"#, turns.join(","))), S(4));
    assert!(m.items.iter().map(|i| i.text().len()).sum::<usize>() <= LIST_MAX);
    assert!(m.items.iter().all(|i| i.text().len() <= PIECE));
    assert!(m.items.first().is_some_and(|i| i.text().starts_with('5')), "the oldest turns are left out: {:?}", m.items.first().map(|i| &i.text()[..2]));
    assert!(m.items.iter().any(|i| i.text().starts_with("11")), "the newest turn is there");
    assert!(m.events.iter().any(|e| e.contains("history is larger")));
    // A full list takes no further question; a new conversation makes room.
    m.bytes = LIST_MAX - 8;
    m.edit.text = "noch eine".into();
    assert!(m.submit(S(5), false).is_empty());
    assert!(m.note.as_ref().is_some_and(|n| n.0.starts_with("Das Gespräch ist zu lang")));
    m.new_conversation(S(6));
    m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":0}"#), S(6));
    assert_eq!(m.submit(S(7), false).len(), 1);
}

#[test]
fn a_paste_keeps_line_ends_and_drops_control_characters() {
    let mut m = ready();
    m.edit.text = "a".into();
    m.edit.cursor = 1;
    m.paste("b\r\nc\td\u{7}e", false);
    assert_eq!(m.edit.text, "ab\nc de");
    assert_eq!(m.edit.cursor, 7);
    assert!(!m.typed_remote);
}

#[test]
fn the_input_line_is_bounded_and_says_so() {
    let mut m = ready();
    m.paste(&"ä".repeat(INPUT_MAX), false);
    assert!(m.edit.text.len() <= INPUT_MAX && m.edit.text.len() > INPUT_MAX - 2);
    assert!(m.note.as_ref().is_some_and(|n| n.0.starts_with("Der Text war zu lang")));
    assert!(!m.insert('x', false));
    assert_eq!(m.note.as_ref().map(|n| n.0.as_str()), Some("Mehr passt nicht in eine Frage."));
}

#[test]
fn a_question_from_injected_input_is_marked_in_the_list() {
    let mut m = ready();
    // `ctl assist ask`: Georg's unsent line stays his, the question carries the mark.
    m.edit.text = "mein Entwurf".into();
    assert_eq!(m.ask("Lösch alles.", S(1), true).len(), 1);
    assert_eq!(m.edit.text, "mein Entwurf");
    assert_eq!(m.items, [Item::Turn { who: Who::Georg, text: "Lösch alles.".into() }, Item::Note { text: REMOTE_ASK.into(), error: false }]);
    assert!(m.events.iter().any(|e| e == "assist: ask 1 from injected input"));
    let id = m.ask_id().unwrap();
    ev(&mut m, &format!(r#"{{"ev":"done","id":{id}}}"#), 2);
    m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":2}"#), S(2));
    // Text that injected input typed stays marked when Georg's own Enter sends it; his own text is not.
    m.edit.text.clear();
    m.edit.cursor = 0;
    m.insert('x', true);
    assert_eq!(m.submit(S(3), false).len(), 1);
    assert_eq!(m.items.last(), Some(&Item::Note { text: REMOTE_ASK.into(), error: false }));
    let id = m.ask_id().unwrap();
    ev(&mut m, &format!(r#"{{"ev":"done","id":{id}}}"#), 4);
    m.on(&j(r#"{"ok":true,"name":"Mira","ready":true,"busy":false,"turns":4}"#), S(4));
    m.insert('y', false);
    assert_eq!(m.submit(S(5), false).len(), 1);
    assert_eq!(m.items.last(), Some(&Item::Turn { who: Who::Georg, text: "y".into() }));
}

#[test]
fn a_delete_asks_with_its_title_and_leaves_the_way_to_the_trash() {
    // Board 5: "Löschen fragt immer" over the question; after it, the line with "Papierkorb zeigen".
    let mut m = ready();
    let id = ask(&mut m, "Räum die Downloads auf.", 1);
    ev(&mut m, &format!(r#"{{"ev":"tool","id":{id},"name":"loeschen","state":"asks","line":"Soll ich die 3 doppelten Dateien löschen?","ask_id":9}}"#), 2);
    let c = m.card.clone().expect("the question");
    assert_eq!((c.title.as_deref(), c.text.as_str(), c.yes_picked), (Some(DELETE_TITLE), "Soll ich die 3 doppelten Dateien löschen?", true));
    assert!(m.trash.is_none());
    ev(&mut m, &format!(r#"{{"ev":"tool","id":{id},"name":"loeschen","state":"done","line":""}}"#), 3);
    assert!(m.trash.is_some(), "the way to the trash's listing");
    m.note_item("Im Papierkorb:\nliste-alt.txt", false);
    assert!(matches!(m.items.last(), Some(Item::Note { text, .. }) if text.contains("liste-alt.txt")));
}
