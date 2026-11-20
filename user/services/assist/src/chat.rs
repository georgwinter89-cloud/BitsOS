//! The full-screen chat on the console (CL-3a; the desktop window of CL-3b replaces this file, nothing below
//! it). Plain ANSI on the alternate screen, so the console looks as before when the chat ends.

use crate::config;
use crate::engine::{Answer, Session, Ui};
use crate::provider::Place;
use crate::settings::Settings;
use crate::term::edit::LineEdit;
use crate::term::keys::{Decoder, Key};
use crate::term::view::{self, Transcript, View, Who};
use crate::term::{Term, sys, wrap};
use crate::tools::ToolSource;
use std::io::Write;
use std::time::{Duration, Instant};

/// Redraws while text streams in come at most this often; the console's framebuffer is not fast.
const REDRAW: Duration = Duration::from_millis(40);
/// A lone Esc is told from the start of an arrow key by what follows within this time. Long enough for a
/// serial line or a remote terminal, where the bytes of one key can arrive apart.
const ESC_WAIT_MS: i32 = 120;
const LEAVE_HINT: &str = "Der angefangene Text geht verloren: noch einmal Esc beendet, jede andere Taste schreibt weiter";

struct Screen<'t> {
    term: &'t mut dyn Term,
    dec: Decoder,
    transcript: Transcript,
    edit: LineEdit,
    scroll: usize,
    name: String,
    status: String,
    busy: bool,
    stop: bool,
    /// Esc was pressed over a line being typed; the next one leaves.
    leaving: bool,
    drawn: Instant,
    log: Option<std::fs::File>,
}

impl<'t> Screen<'t> {
    fn new(term: &'t mut dyn Term, log: Option<std::fs::File>) -> Screen<'t> {
        Screen {
            term,
            dec: Decoder::default(),
            transcript: Transcript::default(),
            edit: LineEdit::default(),
            scroll: 0,
            name: "Assistent".into(),
            status: String::new(),
            busy: false,
            stop: false,
            leaving: false,
            drawn: Instant::now(),
            log,
        }
    }

    fn draw(&mut self) {
        let (cols, rows) = self.term.size();
        self.scroll = self.scroll.min(view::max_scroll(&mut self.transcript, &self.name, cols, rows));
        let status = if self.leaving { LEAVE_HINT } else { &self.status };
        let v = View { name: &self.name, status, input: self.edit.chars(), cursor: self.edit.cursor(), scroll: self.scroll, busy: self.busy };
        let picture = view::frame(&mut self.transcript, &v, cols, rows);
        self.term.write(picture.as_bytes());
        self.drawn = Instant::now();
    }

    /// Keys typed so far; waits up to `ms` for the first one (-1: until one comes). None: stdin ended.
    fn keys(&mut self, ms: i32) -> Option<Vec<Key>> {
        let mut out = Vec::new();
        let mut wait = ms;
        loop {
            if !self.term.wait_input(wait) {
                out.extend(self.dec.idle());
                return Some(out);
            }
            let mut buf = [0u8; 256];
            let n = self.term.read_input(&mut buf);
            if n == 0 {
                return if out.is_empty() { None } else { Some(out) };
            }
            out.extend(self.dec.feed(&buf[..n]));
            if !self.dec.waiting() {
                return Some(out);
            }
            wait = ESC_WAIT_MS;
        }
    }

    fn page(&mut self, up: bool) {
        let step = self.term.size().1.saturating_sub(3).max(1);
        self.scroll = if up { self.scroll + step } else { self.scroll.saturating_sub(step) };
    }

    fn status(&mut self, settings: &dyn Settings) {
        self.status = match config::load(settings) {
            Err(_) => "Einstellungen nicht erreichbar".into(),
            Ok(cfg) => {
                self.name = cfg.name.clone();
                let who = match cfg.provider(&cfg.chat) {
                    Ok(p) => format!("{} · {} · {}", p.id, if p.model.is_empty() { "Hilfsprogramm" } else { &p.model }, if p.place == Place::Local { "lokal" } else { "Cloud" }),
                    Err(_) => "kein Anbieter".into(),
                };
                let local = if cfg.local_only { " · Nur lokal" } else { "" };
                format!("{who}{local}   Esc beendet · Bild↑/Bild↓ blättert")
            }
        };
    }
}

impl Ui for Screen<'_> {
    fn text(&mut self, delta: &str) {
        self.transcript.stream(delta);
        if self.drawn.elapsed() >= REDRAW {
            self.draw();
        }
    }

    fn note(&mut self, line: &str) {
        self.transcript.push(Who::Note, line);
        self.draw();
    }

    fn log(&mut self, line: &str) {
        self.transcript.push(Who::Tech, line);
        if let Some(f) = &mut self.log {
            let _ = writeln!(f, "{}", wrap::clean(line));
        }
        self.draw();
    }

    fn ask(&mut self, question: &str) -> Answer {
        // The question is one line, whatever it quotes; the line below it is always this program's own.
        self.transcript.push(Who::Question, &format!("{}\nErlauben? (j = ja, n = nein)", question.replace('\n', " ")));
        self.scroll = 0;
        self.draw();
        // Only a key pressed with the question on the screen answers it: whatever was typed before (the next
        // question begun early, a second press of j for the call before) is forgotten, not taken as a yes.
        self.term.drop_input();
        self.dec = Decoder::default();
        loop {
            let Some(keys) = self.keys(-1) else { return Answer::No };
            for k in keys {
                let answer = match k {
                    Key::Char('j' | 'J' | 'y' | 'Y') => Answer::Yes,
                    Key::Char('n' | 'N') | Key::Esc | Key::Ctrl('c' | 'd') => Answer::No,
                    _ => continue,
                };
                self.transcript.append(if answer == Answer::Yes { " ja" } else { " nein" });
                self.draw();
                return answer;
            }
        }
    }

    /// Looks at the keyboard while an answer runs: Esc stops it, the page keys scroll, anything else is
    /// already the next question being typed.
    fn cancelled(&mut self) -> bool {
        if self.stop || !self.term.wait_input(0) {
            return self.stop;
        }
        let mut changed = false;
        for k in self.keys(0).unwrap_or_default() {
            match k {
                Key::Esc | Key::Ctrl('c') => self.stop = true,
                Key::PageUp | Key::PageDown => {
                    self.page(k == Key::PageUp);
                    changed = true;
                }
                Key::Enter => {}
                k => changed |= self.edit.key(k),
            }
        }
        if changed {
            self.draw();
        }
        self.stop
    }
}

pub fn run(settings: &dyn Settings, tools: &dyn ToolSource, budget: usize, log: Option<std::fs::File>) -> i32 {
    if !sys::is_tty(0) || !sys::is_tty(1) {
        eprintln!("assist: Das Gespräch braucht eine Konsole. Für Skripte: assist --once \"TEXT\"");
        return 2;
    }
    let _restore = sys::raw();
    let mut console = sys::Console;
    console.write(b"\x1b[?1049h\x1b[2J");
    talk(&mut console, settings, tools, budget, log);
    console.write(b"\x1b[?1049l");
    0
}

/// The chat on `term` until Georg leaves it.
fn talk(term: &mut dyn Term, settings: &dyn Settings, tools: &dyn ToolSource, budget: usize, log: Option<std::fs::File>) {
    let mut session = Session::new(settings, tools, budget);
    let mut s = Screen::new(term, log);
    s.status(settings);
    let (note, log) = tools.greeting();
    if let Some(n) = note {
        s.transcript.push(Who::Note, &n);
    }
    if let Some(l) = log {
        s.transcript.push(Who::Tech, &l);
        if let Some(f) = &mut s.log {
            let _ = writeln!(f, "{}", wrap::clean(&l));
        }
    }
    if let Ok(cfg) = config::load(settings)
        && let Err(how) = cfg.candidates()
    {
        s.transcript.push(Who::Note, &how);
        // Longer than a small screen: it is read from its first line (draw() clamps this to the top).
        s.scroll = usize::MAX;
    }
    s.draw();
    'chat: loop {
        let Some(keys) = s.keys(-1) else { break };
        for k in keys {
            let leave = matches!(k, Key::Esc | Key::Ctrl('d'));
            // A slip of the finger must not cost a long question: over a line being typed, leaving takes two.
            if leave && (s.edit.is_empty() || s.leaving) {
                break 'chat;
            }
            s.leaving = leave;
            match k {
                Key::Esc | Key::Ctrl('d') => {}
                Key::PageUp | Key::PageDown => s.page(k == Key::PageUp),
                Key::Ctrl('l') => s.term.write(b"\x1b[2J"),
                Key::Enter if !s.edit.is_empty() => {
                    let text = s.edit.take();
                    s.transcript.push(Who::User, &text);
                    (s.busy, s.stop, s.scroll) = (true, false, 0);
                    s.draw();
                    if let Err(why) = session.ask(text.trim(), &mut s) {
                        s.transcript.push(Who::Note, &why);
                    }
                    s.busy = false;
                    s.status(settings);
                }
                k => {
                    s.edit.key(k);
                }
            }
        }
        s.draw();
    }
}

#[cfg(test)]
#[path = "chat_tests.rs"]
mod tests;
