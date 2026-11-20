//! `assist --once "TEXT"`: one question, the answer on stdout, everything else on stderr. For scripts and
//! tests: nobody can be asked, so a tool call that needs a yes is refused and said so. Exit code 0: a whole
//! answer; 1: no answer (the reason on stderr); 3: an answer that was cut, refused, ended early or is empty.

use crate::conv::Stop;
use crate::engine::{Answer, Session, Ui};
use crate::settings::Settings;
use crate::term::wrap;
use crate::tools::ToolSource;
use std::io::Write;

struct Plain {
    ends_with_newline: bool,
}

impl Ui for Plain {
    fn text(&mut self, delta: &str) {
        // The answer is data: no control character of it reaches a terminal behind stdout.
        let clean = wrap::clean(delta);
        if clean.is_empty() {
            return;
        }
        self.ends_with_newline = clean.ends_with('\n');
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(clean.as_bytes());
        let _ = out.flush();
    }
    fn note(&mut self, line: &str) {
        eprintln!("assist: {}", wrap::clean(line));
    }
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

pub fn run(settings: &dyn Settings, tools: &dyn ToolSource, budget: usize, text: &str) -> i32 {
    let mut ui = Plain { ends_with_newline: true };
    let (note, log) = tools.greeting();
    if let Some(n) = note {
        ui.note(&n);
    }
    if let Some(l) = log {
        ui.log(&l);
    }
    let r = Session::new(settings, tools, budget).ask(text, &mut ui);
    if !ui.ends_with_newline {
        println!();
    }
    match r {
        Ok(Stop::End) => 0,
        // Cut at the length limit, refused, ended early or empty (said on stderr): a script must not take
        // what is on stdout for a whole answer.
        Ok(_) => 3,
        Err(why) => {
            eprintln!("{why}");
            1
        }
    }
}
