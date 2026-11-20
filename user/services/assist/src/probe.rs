//! `assist --test NAME` (CL-5s): is the provider NAME set up so that it answers? ONE smallest request goes to
//! it, with its key, exactly as a question in the chat would: the same rules decide whether it may be
//! contacted at all ("Nur lokal", http only at home). No second try, no fallback, no tools.
//!
//! stdout carries one German line, for Georg at the console and for the settings page "KI", which shows it as
//! it is; technical lines go to stderr. Exit code 0: reached, and the model answered. 1: asked, but no answer
//! (away, key refused, model unknown, too slow). 2: not asked (not set up completely, or a rule forbids it).
//! The key never appears in either stream.
//!
//! The whole test has one wait ([`PATIENCE`]), counted from its start: reading the settings and the key
//! comes out of it, the request gets what is left. The page can so give the program that long plus a little.

use crate::config;
use crate::conv::Message;
use crate::engine::{Answer, Session, Ui};
use crate::provider::{ErrKind, Kind};
use crate::settings::Settings;
use crate::term::wrap;
use crate::tools::ToolSource;
use std::time::{Duration, Instant};

/// What is asked: the shortest thing that makes a model say something.
const QUESTION: &str = "Antworte nur mit dem Wort: ok";
/// Room for that one word.
const ANSWER_TOKENS: u64 = 16;
/// The page waits for the line. A model that loads first may need longer; then Georg tests again.
pub const PATIENCE: Duration = Duration::from_secs(60);

/// The model's words are not shown: only whether any came.
struct Quiet {
    answered: bool,
}

impl Ui for Quiet {
    fn text(&mut self, delta: &str) {
        self.answered |= !delta.trim().is_empty();
    }
    fn note(&mut self, _line: &str) {}
    fn log(&mut self, line: &str) {
        eprintln!("assist: log {}", wrap::clean(line));
    }
    fn ask(&mut self, _question: &str) -> Answer {
        Answer::CannotAsk
    }
    fn cancelled(&mut self) -> bool {
        false
    }
}

/// One line, whatever a sentence or a helper program brought.
fn line(text: &str) -> String {
    wrap::clean(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn say(code: i32, text: &str) -> i32 {
    println!("{}", line(text));
    code
}

/// The request's own wait: what is left of the test's, at most `ai.timeout_s`. None: nothing is left.
fn wait(timeout: Duration, until: Instant) -> Option<Duration> {
    let left = until.saturating_duration_since(Instant::now());
    // settingsd allows no `ai.timeout_s` under 1; a zero from anywhere else would fail every test.
    (!left.is_zero()).then(|| timeout.max(Duration::from_secs(1)).min(left))
}

/// `settings` gives up at `until` as well ([`crate::settings::Socket::until`]).
pub fn run(settings: &dyn Settings, tools: &dyn ToolSource, budget: usize, id: &str, until: Instant) -> i32 {
    let slow = "Die Einstellungen haben zu lange gebraucht, der Anbieter wurde nicht gefragt. Teste gleich noch einmal.";
    let mut cfg = match config::load(settings) {
        Ok(c) => c,
        Err(_) if Instant::now() >= until => return say(2, slow),
        Err(e) => return say(2, &format!("Die Einstellungen sind nicht erreichbar ({}). Läuft settingsd?", e.code)),
    };
    cfg.max_tokens = ANSWER_TOKENS;
    let p = match cfg.provider(id).and_then(|p| cfg.admit(&p).map(|_| p)).and_then(|p| config::with_key(settings, p)) {
        Ok(p) => p,
        Err(_) if Instant::now() >= until => return say(2, slow),
        Err(why) => return say(2, &why),
    };
    // Only now: reading the key came out of the same wait.
    let Some(wait) = wait(cfg.timeout, until) else { return say(2, slow) };
    cfg.timeout = wait;
    let mut session = Session::new(settings, tools, budget);
    session.conv.messages.push(Message::user(QUESTION));
    let mut ui = Quiet { answered: false };
    let who = if p.kind == Kind::Program { format!("Das Hilfsprogramm von „{}“", p.id) } else { format!("Der Anbieter „{}“", p.id) };
    let model = if p.model.is_empty() { " und".to_string() } else { format!(", das Modell „{}“", p.model) };
    match session.single(&cfg, &p, &mut ui) {
        Ok(_) if ui.answered => say(0, &format!("{who} ist erreichbar{model} hat geantwortet.")),
        Ok(_) => say(1, &format!("{who} ist erreichbar{model} hat aber nichts geantwortet.")),
        // The usual sentence names ai.timeout_s; here the test's own, shorter wait may have ended it.
        Err(e) if e.kind == Some(ErrKind::Timeout) => say(
            1,
            &format!("{who} hat in {} Sekunden nicht geantwortet. Lädt dort gerade ein Modell, dann teste gleich noch einmal.", wait.as_secs_f64().ceil()),
        ),
        Err(e) => say(1, &e.why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_gets_what_is_left_of_the_tests_wait() {
        let s = Duration::from_secs;
        let now = Instant::now();
        assert_eq!(wait(s(600), now), None, "nothing left: the provider is not asked");
        let w = wait(s(600), now + s(40)).unwrap();
        assert!(w <= s(40) && w > s(39), "{w:?}");
        assert_eq!(wait(s(5), now + s(40)), Some(s(5)), "a smaller ai.timeout_s counts");
        assert_eq!(wait(s(0), now + s(40)), Some(s(1)), "never a wait of nothing");
    }
}
