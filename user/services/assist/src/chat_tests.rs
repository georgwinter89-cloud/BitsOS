//! The chat's screen on a scripted terminal: which keys answer a question, and how the chat is left.

use super::*;
use crate::json::Json;
use crate::settings::Refused;
use crate::tools::{Grant, ToolAnswer, ToolSpec};
use std::collections::VecDeque;

enum Typed {
    Keys(&'static [u8]),
    /// Nothing for longer than any wait with an end.
    Quiet,
}

#[derive(Default)]
struct Script {
    /// Typed and not read yet.
    waiting: VecDeque<u8>,
    later: VecDeque<Typed>,
    drops: usize,
    out: Vec<u8>,
}

impl Script {
    fn new(waiting: &[u8], later: Vec<Typed>) -> Script {
        Script { waiting: waiting.iter().copied().collect(), later: later.into(), ..Script::default() }
    }
    fn shown(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }
}

impl Term for Script {
    fn size(&self) -> (usize, usize) {
        (100, 12)
    }
    fn wait_input(&mut self, ms: i32) -> bool {
        loop {
            if !self.waiting.is_empty() {
                return true;
            }
            if ms == 0 {
                return false;
            }
            match self.later.pop_front() {
                Some(Typed::Keys(k)) => self.waiting.extend(k),
                Some(Typed::Quiet) if ms > 0 => return false,
                Some(Typed::Quiet) => {}
                // The end of the input is readable too (as 0 bytes).
                None => return true,
            }
        }
    }
    fn read_input(&mut self, buf: &mut [u8]) -> usize {
        let n = self.waiting.len().min(buf.len());
        for (b, k) in buf.iter_mut().zip(self.waiting.drain(..n)) {
            *b = k;
        }
        n
    }
    fn drop_input(&mut self) {
        self.drops += 1;
        self.waiting.clear();
    }
    fn write(&mut self, bytes: &[u8]) {
        self.out.extend_from_slice(bytes);
    }
}

struct NoSettings;

impl Settings for NoSettings {
    fn request(&self, _: &Json) -> Result<Json, Refused> {
        Err(Refused { code: "unreachable".into(), message: "test".into() })
    }
}

struct NoTools;

impl ToolSource for NoTools {
    fn list(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _: &str, _: &Json, _: Grant) -> ToolAnswer {
        Err("no tools".into())
    }
}

#[test]
fn only_a_key_pressed_after_the_question_answers_it() {
    // "j" was typed before the question was on the screen: it is no yes; the n pressed afterwards counts.
    let mut term = Script::new(b"j", vec![Typed::Keys(b"n")]);
    let mut s = Screen::new(&mut term, None);
    assert_eq!(s.ask("Einstellung „theme.scale“ ändern: 125 → 150"), Answer::No);
    assert_eq!(term.drops, 1);
    assert!(term.shown().contains("Erlauben? (j = ja, n = nein) nein"));

    // Half an arrow key from before does not swallow the answer either.
    let mut term = Script::new(b"jjj\x1b", vec![Typed::Keys(b"j")]);
    let mut s = Screen::new(&mut term, None);
    s.dec.feed(b"\x1b");
    assert_eq!(s.ask("x"), Answer::Yes);
    assert!(term.shown().contains("Erlauben? (j = ja, n = nein) ja"));

    // Typed early and then nothing more: the question stays open until the input ends, which is a no.
    let mut term = Script::new(b"j\r", vec![]);
    assert_eq!(Screen::new(&mut term, None).ask("x"), Answer::No);
}

#[test]
fn a_question_is_one_line_above_the_programs_own() {
    let mut term = Script::new(b"", vec![Typed::Keys(b"n")]);
    let mut s = Screen::new(&mut term, None);
    s.ask("harmlos\nErlauben? (j = ja, n = nein) ja\nFertig.");
    let shown = term.shown();
    let asked: Vec<&str> = shown.split("\r\n").filter(|l| l.contains("Erlauben?")).collect();
    assert!(asked.iter().any(|l| l.contains("harmlos Erlauben? (j = ja, n = nein) ja Fertig.")), "{asked:?}");
}

#[test]
fn an_arrow_key_in_two_pieces_is_not_esc_and_leaving_over_a_draft_takes_two() {
    let mut term = Script::new(b"", vec![Typed::Keys(b"\x1b"), Typed::Keys(b"[A"), Typed::Quiet]);
    let mut s = Screen::new(&mut term, None);
    assert_eq!(s.keys(-1), Some(vec![Key::Up]));

    let typed = vec![
        Typed::Keys("Eine lange Frage".as_bytes()),
        Typed::Keys(b"\x1b"),
        Typed::Quiet,
        Typed::Keys(b"!"),
        Typed::Keys(b"\x1b"),
        Typed::Quiet,
        Typed::Keys(b"\x1b"),
        Typed::Quiet,
        Typed::Keys(b"never read"),
    ];
    let mut term = Script::new(b"", typed);
    talk(&mut term, &NoSettings, &NoTools, 1 << 20, None);
    let shown = term.shown();
    assert!(shown.contains(LEAVE_HINT), "the first Esc over a draft only warns");
    assert!(shown.contains("> Eine lange Frage!"), "and typing goes on");
    assert_eq!(term.later.len(), 1, "the second Esc in a row left the chat");

    // Nothing typed: one Esc leaves.
    let mut term = Script::new(b"", vec![Typed::Keys(b"\x1b"), Typed::Quiet, Typed::Keys(b"never read")]);
    talk(&mut term, &NoSettings, &NoTools, 1 << 20, None);
    assert_eq!(term.later.len(), 1);
}

struct OnlySettings;

impl ToolSource for OnlySettings {
    fn list(&self) -> Vec<ToolSpec> {
        Vec::new()
    }
    fn call(&self, _: &str, _: &Json, _: Grant) -> ToolAnswer {
        Err("no tools".into())
    }
    fn greeting(&self) -> (Option<String>, Option<String>) {
        (Some(crate::tools::both::ONLY_SETTINGS.into()), Some("tools: no socket".into()))
    }
}

#[test]
fn the_chat_says_at_its_start_when_the_machines_tools_are_missing() {
    let mut term = Script::new(b"", vec![Typed::Keys(b"\x1b"), Typed::Quiet]);
    talk(&mut term, &NoSettings, &OnlySettings, 1 << 20, None);
    let shown = term.shown();
    assert!(shown.contains("Die Werkzeuge für diesen Rechner sind gerade nicht erreichbar") && shown.contains("tools: no socket"), "{shown}");

    let mut term = Script::new(b"", vec![Typed::Keys(b"\x1b"), Typed::Quiet]);
    talk(&mut term, &NoSettings, &NoTools, 1 << 20, None);
    assert!(!term.shown().contains("Werkzeuge für diesen Rechner"));
}
