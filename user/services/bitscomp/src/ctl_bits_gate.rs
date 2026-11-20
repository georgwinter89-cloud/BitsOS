//! EINS-D (PLAN K5, K7): a control socket peer inside the assistant's process tree is the assistant. Its lines are
//! judged as the assistant's own desktop tool: reading runs at every level (not for a program on the list
//! `assistant.never_see`); a change is refused at `answer`, asked at `ask`, runs at `auto`; a step that buys, sends
//! or cannot be undone (`db_act::classify` Final, F2) is asked at every level; F1 (`agent_limits`: no answering
//! button, no secret or payment field, no confirm setting) holds always. Anything not listed here is refused.
//!
//! A question gets a number and waits `WAIT` for Georg. `answer` takes the yes only from his real hand: the
//! sidebar's card (`ctl_bits_card`, the card rules of `assist_card`) calls it with `Origin::Physical`; without a
//! sidebar on the screen the question is a notice and runs out (refused). While the service runs only questions from outside (`by:"remote"`) and none of
//! Georg's own hand, the tree gets no more than `ask` (PLAN K3).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use uilist::{Action, Element, Window};

use crate::input::Origin;
use crate::state::App;
use crate::ui_reg::Full;

/// How long a question stays open (the sidebar's QUESTION_WAIT).
pub const WAIT: Duration = Duration::from_secs(120);
/// Questions older than this are forgotten.
const KEEP: Duration = Duration::from_secs(600);

pub const ANSWER_ONLY: &str = "Auf der Stufe „Nur antworten“ handelt der Assistent nicht. Es wurde nichts getan.";
pub const REAL_ONLY: &str = "Ja geht nur mit Georgs echter Maus oder Tastatur.";
pub const NEVER_SEE: &str = "Dieses Programm steht auf der Liste „bei diesen Programmen nie“: der Assistent sieht und bedient es nicht.";
pub const NOT_FROM_TREE: &str = "Den Papierkorb leert nur Georg selbst, nie der Assistent. Es wurde nichts gelöscht.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Yes,
    No,
    Used,
}

#[derive(Clone, Debug)]
pub struct Question {
    pub n: u64,
    /// The asker (SO_PEERCRED): only it may run the line after the yes.
    pub pid: i32,
    /// The control line that runs once after the yes; None: the asker acts itself (a setting, the trash).
    pub line: Option<String>,
    /// The sentence Georg reads (without who asks).
    pub text: String,
    /// Asked by a peer in the assistant's tree (else by a command of Georg's own, `bits papierkorb leeren`).
    pub from_tree: bool,
    pub at: Instant,
    pub state: State,
}

thread_local! {
    static QUESTIONS: RefCell<(u64, Vec<Question>)> = const { RefCell::new((0, Vec::new())) };
}

/// The open questions (wave EINS-B shows them as cards).
pub fn pending() -> Vec<Question> {
    QUESTIONS.with(|q| q.borrow().1.iter().filter(|x| x.state == State::Waiting && x.at.elapsed() < WAIT).cloned().collect())
}

/// How a question starts on the screen: who asks.
pub const BY_ASSISTANT: &str = "Der Assistent möchte ";
pub const BY_COMMAND: &str = "Ein Befehl möchte ";

pub fn open(app: &mut App, pid: i32, line: Option<String>, text: String, from_tree: bool) -> u64 {
    let n = QUESTIONS.with(|q| {
        let mut q = q.borrow_mut();
        q.1.retain(|x| x.at.elapsed() < KEEP);
        q.0 += 1;
        let n = q.0;
        q.1.push(Question { n, pid, line, text: text.clone(), from_tree, at: Instant::now(), state: State::Waiting });
        n
    });
    eprintln!("bitscomp: bits: question {n} for pid {pid}");
    app.notice_for(format!("{}{text}", if from_tree { BY_ASSISTANT } else { BY_COMMAND }), WAIT);
    app.bits_card_show();
    n
}

/// Georg's answer (or anyone's no). Err: why it does not count.
pub fn answer(app: &mut App, n: u64, yes: bool, origin: Origin) -> Result<(), String> {
    if yes && origin != Origin::Physical {
        return Err(REAL_ONLY.into());
    }
    let r = QUESTIONS.with(|q| {
        let mut q = q.borrow_mut();
        let x = q.1.iter_mut().find(|x| x.n == n).ok_or("Diese Frage gibt es nicht (mehr).")?;
        if x.state != State::Waiting || x.at.elapsed() >= WAIT {
            return Err("Diese Frage ist schon beantwortet oder abgelaufen.");
        }
        x.state = if yes { State::Yes } else { State::No };
        Ok(())
    });
    if r.is_ok() {
        eprintln!("bitscomp: bits: question {n} answered {}", if yes { "yes" } else { "no" });
        if app.desk.notice.as_ref().is_some_and(|(t, _)| t.starts_with(BY_ASSISTANT) || t.starts_with(BY_COMMAND)) {
            app.desk.notice = None;
            app.chrome_dirty();
        }
    }
    r.map_err(str::to_string)
}

/// `bits-antwort <n>`: wartet, ja, nein, abgelaufen, unbekannt.
pub fn status(n: u64) -> &'static str {
    QUESTIONS.with(|q| match q.borrow().1.iter().find(|x| x.n == n) {
        None => "unbekannt",
        Some(x) if x.state == State::Waiting && x.at.elapsed() >= WAIT => "abgelaufen",
        Some(x) => match x.state {
            State::Waiting => "wartet",
            State::Yes => "ja",
            State::No => "nein",
            State::Used => "abgelaufen",
        },
    })
}

/// `bits-mit <n> <line>`: the yes of question n for exactly this line and this asker, once.
pub fn take_yes(n: u64, pid: i32, line: &str) -> Result<(), String> {
    QUESTIONS.with(|q| {
        let mut q = q.borrow_mut();
        let x = q.1.iter_mut().find(|x| x.n == n).ok_or("Diese Frage gibt es nicht (mehr).")?;
        if x.state != State::Yes || x.pid != pid || x.line.as_deref() != Some(line) || x.at.elapsed() >= WAIT * 2 {
            return Err("Für diesen Schritt hat Georg nicht Ja gesagt. Es wurde nichts getan.");
        }
        x.state = State::Used;
        Ok(())
    })
    .map_err(str::to_string)
}

/// What a line from the tree gets.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Pass,
    Ask(String),
    Refuse(String),
}

/// PLAN K2's level (`ai.level`; `ask` while nobody set it), for the tree: never more than `ask` while the
/// service's running questions all came from outside (K3: a remote question never gets more than ask).
pub fn level(app: &App) -> &str {
    let hand = &app.assist_route.hand;
    match app.settings.text("ai.level") {
        Some("auto") if !hand.is_empty() && !hand.values().any(|h| *h) => "ask",
        Some(l @ ("answer" | "ask" | "auto")) => l,
        _ => "ask",
    }
}

pub fn never_seen(app: &App, program: &str) -> bool {
    let p = program.to_lowercase();
    app.settings.list("assistant.never_see").unwrap_or_default().iter().any(|w| !w.is_empty() && p.contains(&w.to_lowercase()))
}

const READS: &[&str] = &["list", "ui-history", "program-find", "theme", "clock", "bars", "cursor", "hot", "xwayland", "bits-clip-get", "bits-antwort", "bits-meldung", "bits-foto"];

/// A tree peer's line (the bits verbs' own checks are in ctl_bits).
pub fn judge(app: &App, words: &[&str], line: &str) -> Verdict {
    let verb = words.first().copied().unwrap_or("");
    let one = words.len() == 1;
    let change = |text: String| match level(app) {
        "answer" => Verdict::Refuse(ANSWER_ONLY.into()),
        "auto" => Verdict::Pass,
        _ => Verdict::Ask(text),
    };
    match verb {
        v if READS.contains(&v) => Verdict::Pass,
        // Runs only the line Georg said yes to, for its asker, once (`take_yes`).
        "bits-mit" => Verdict::Pass,
        "notice" | "stats" | "page" if one => Verdict::Pass,
        // `<thing> state`: what a part of the desktop shows (EINS-B's reads).
        _ if words.get(1) == Some(&"state") && words.len() == 2 => Verdict::Pass,
        "library" if one || words.get(1) == Some(&"search") => Verdict::Pass,
        "ui" => {
            let rest = if words.get(1) == Some(&"text") { &words[2..] } else { &words[1..] };
            let w = rest.first().map_or_else(|| app.ui_focused_id(), |w| if *w == "focus" { app.ui_focused_id() } else { w.to_string() });
            match app.ui_windows(&Full::Local(w.clone())).into_iter().find(|x| x.window == w) {
                Some(x) if never_seen(app, &x.app) => Verdict::Refuse(NEVER_SEE.into()),
                _ => Verdict::Pass,
            }
        }
        "ui-act" => act(app, words, line, change, None),
        // `bits ui tun --rueckgaengig|--endgueltig`: the caller's own word whether the step can be undone, as the
        // model's `unumkehrbar` in Durchblick (without it a press is "not sure" and asked at every level).
        "bits-schritt" => match (words.get(1).copied(), step_line(line)) {
            (Some("rueckgaengig"), Some(inner)) => act(app, &words[2..], inner, change, Some(false)),
            (Some("endgueltig"), Some(inner)) => act(app, &words[2..], inner, change, Some(true)),
            _ => Verdict::Refuse("bits-schritt rueckgaengig|endgueltig ui-act ...".into()),
        },
        "program-start" => change(format!("{} starten.", line.trim_start().strip_prefix("program-start").unwrap_or("").trim())),
        "close" | "bits-vor" | "bits-groesse" => {
            let title = words.get(1).and_then(|id| id.parse::<u64>().ok()).and_then(|id| app.all_windows().into_iter().find(|w| crate::ctl::win_id(w) == id)).map(|w| crate::ctl::names(&w).1).unwrap_or_default();
            let what = match verb {
                "close" => "schließen",
                "bits-vor" => "nach vorn holen",
                _ => "in der Größe ändern",
            };
            change(format!("das Fenster „{title}“ {what}."))
        }
        "bits-clip-set" => change("etwas in die Zwischenablage legen.".into()),
        "bits-frage" => match words.get(1).copied() {
            Some("leeren") => Verdict::Refuse(NOT_FROM_TREE.into()),
            _ => Verdict::Pass,
        },
        _ => Verdict::Refuse(format!("Aus dem Assistenten heraus geht „{verb}“ nicht; „bits hilfe“ zeigt, was geht.")),
    }
}

/// The `ui-act ...` inside `bits-schritt <word> ui-act ...`.
pub fn step_line(line: &str) -> Option<&str> {
    let inner = line.trim_start().splitn(3, char::is_whitespace).nth(2)?.trim_start();
    inner.starts_with("ui-act ").then_some(inner)
}

fn act(app: &App, words: &[&str], line: &str, change: impl Fn(String) -> Verdict, flag: Option<bool>) -> Verdict {
    if words.first() != Some(&"ui-act") {
        return Verdict::Refuse("bits-schritt gilt nur für ui-act.".into());
    }
    let (Some(id), Some(action)) = (words.get(1), words.get(2)) else { return Verdict::Pass };
    let Some(a) = Action::of_word(action) else { return Verdict::Pass };
    let text = line.trim_start().splitn(4, char::is_whitespace).nth(3).map(str::trim_start).filter(|t| !t.is_empty()).map(|t| t.replace("\\n", "\n"));
    let (w, e) = match element(app, id) {
        Ok(we) => we,
        Err(why) => return Verdict::Refuse(why),
    };
    if never_seen(app, &w.app) {
        return Verdict::Refuse(NEVER_SEE.into());
    }
    if let Err(why) = crate::ui_act::agent_limits(crate::ui_act::By::Mcp, &w, &e, a, text.as_deref()) {
        return Verdict::Refuse(why);
    }
    let step = crate::db_act::Step { element: e.id.clone(), action: a, text: text.clone(), name: e.name.clone(), flag, why: String::new() };
    let said = describe(&w, &e, a, text.as_deref());
    match crate::db_act::classify(&w, &e, &step) {
        crate::db_act::Risk::Final(why) if level(app) != "answer" => Verdict::Ask(format!("{said} {}", reason(why))),
        _ => change(said),
    }
}

/// classify's words as a sentence.
fn reason(why: &str) -> String {
    if why == crate::db_act::UNSURE {
        return "Ob es sich rückgängig machen lässt, ist nicht sicher.".into();
    }
    match why.strip_prefix("das ").or_else(|| why.strip_prefix("der ")) {
        Some(_) => {
            let mut c = why.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str() + ".").unwrap_or_default()
        }
        None => format!("Das {why}."),
    }
}

/// The window and element of an id as the desktop knows them now (the provider's last list for a foreign window).
pub fn element(app: &App, id: &str) -> Result<(Window, Element), String> {
    let id = uilist::clean_id(id);
    let win = uilist::window_of(id).ok_or_else(|| format!("„{id}“ ist keine Element-Nummer (sie sieht aus wie w3/1a2b)."))?;
    let w = app.ui_windows(&Full::Local(win.to_string())).into_iter().find(|w| w.window == win).ok_or_else(|| format!("Das Fenster {win} gibt es nicht mehr."))?;
    let e = w.find(id).cloned().ok_or_else(|| format!("Das Element {id} kennt die Oberfläche nicht; „bits ui liste {win}“ liest das Fenster neu."))?;
    Ok((w, e))
}

fn describe(w: &Window, e: &Element, a: Action, text: Option<&str>) -> String {
    let what = match a {
        Action::Type => format!("„{}“ in „{}“ tippen", text.unwrap_or(""), e.name),
        Action::Submit => format!("„{}“ abschicken", e.name),
        Action::Scroll => format!("in „{}“ blättern", e.name),
        Action::Toggle => format!("„{}“ umschalten", e.name),
        Action::Choose => format!("„{}“ wählen", e.name),
        Action::Focus => format!("zu „{}“ gehen", e.name),
        Action::Press => format!("„{}“ drücken", e.name),
    };
    format!("in „{}“ {what}.", if w.title.is_empty() { &w.app } else { &w.title })
}

/// A `bits-frage` question's sentence.
pub fn frage_text(words: &[&str]) -> Option<String> {
    let rest = |i: usize| words.get(i..).map(|w| w.join(" ")).unwrap_or_default();
    Some(match words.get(1).copied()? {
        "einstellung" => format!("die Einstellung {} auf „{}“ setzen.", words.get(2)?, rest(3)),
        "zurueck" => format!("„{}“ aus dem Papierkorb zurücklegen.", rest(3)),
        "leeren" => {
            let bytes: f64 = words.get(3)?.parse().ok()?;
            format!("den Papierkorb endgültig leeren ({} Einträge, {:.1} MB)? Das lässt sich nicht rückgängig machen.", words.get(2)?, bytes / 1_048_576.0)
        }
        _ => return None,
    })
}

/// Before a line runs on a yes: the screen may have changed since the question, so a step is checked again.
pub fn recheck(app: &App, line: &str) -> Result<(), String> {
    let line = if line.starts_with("bits-schritt ") { step_line(line).unwrap_or("") } else { line };
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.first() != Some(&"ui-act") {
        return Ok(());
    }
    let (Some(id), Some(a)) = (words.get(1), words.get(2).and_then(|a| Action::of_word(a))) else { return Ok(()) };
    let text = line.trim_start().splitn(4, char::is_whitespace).nth(3).map(str::trim_start).filter(|t| !t.is_empty()).map(|t| t.replace("\\n", "\n"));
    let (w, e) = element(app, id)?;
    if never_seen(app, &w.app) {
        return Err(NEVER_SEE.into());
    }
    crate::ui_act::agent_limits(crate::ui_act::By::Mcp, &w, &e, a, text.as_deref())
}

#[cfg(test)]
#[path = "ctl_bits_gate_tests.rs"]
mod tests;
