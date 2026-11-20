//! DB3, EINS-B (PLAN K4): what goes with a question about a window, built by BitsOS and not by the model. Pure,
//! so the host tests can say what goes in and what never does: the one window's list (secret values never, the
//! element under the pointer or with the keyboard marked), whether its picture goes along, and the program's
//! access log only when the question is about what a program did. As the service's `context`: `{window, list,
//! image, extras}`. A window of a never-see program gives no context at all (`assist_level`).

use mcpreg::Json;
use uilist::{Element, State, Window};

/// What went out with a question (the journal's words).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Liste,
    Zugriffe,
    Bild,
}

impl Kind {
    pub fn word(self) -> &'static str {
        match self {
            Kind::Liste => "liste",
            Kind::Zugriffe => "zugriffe",
            Kind::Bild => "bild",
        }
    }
}

pub struct Inputs<'a> {
    pub window: &'a Window,
    /// The element under the pointer or with the keyboard.
    pub pointed: Option<&'a str>,
    /// The access log's summary, already as lines (None: no access log, or the question is not about it).
    pub access: Option<&'a str>,
    pub picture: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    /// The window's list as text.
    pub list: String,
    /// What else goes along (the access log).
    pub extras: String,
    pub kinds: Vec<Kind>,
}

impl Question {
    /// K4's `context` for window `w`; `image`: the picture as base64 PNG.
    pub fn context(&self, w: &Window, image: Option<String>) -> Json {
        let mut v = vec![("window", Json::obj(vec![("id", Json::str(&w.window)), ("app", Json::str(&w.app)), ("title", Json::str(&w.title))]))];
        if !self.list.is_empty() {
            v.push(("list", Json::str(&self.list)));
        }
        if let Some(data) = image {
            v.push(("image", Json::obj(vec![("media_type", Json::str("image/png")), ("data", Json::Str(data))])));
        }
        if !self.extras.is_empty() {
            v.push(("extras", Json::str(&self.extras)));
        }
        Json::obj(v)
    }

    /// The journal's words for what went out.
    pub fn words(&self) -> String {
        if self.kinds.is_empty() { "-".into() } else { self.kinds.iter().map(|k| k.word()).collect::<Vec<_>>().join(",") }
    }
}

/// Typed with the empty input line (board 1).
pub const DEFAULT: &str = "Erklär mir, was ich hier sehe";
/// The list's text stops here (a page of a few hundred rows is far below).
const LIST_MAX: usize = 48 * 1024;
const ACCESS_MAX: usize = 4 * 1024;

/// Words that make a question one about what a program did: then its access log may go along.
const ABOUT_PROGRAM: [&str; 14] = [
    "was hat", "was macht", "gemacht", "getan", "zugriff", "zugegriffen", "gelesen", "geschrieben", "verbunden", "netz", "internet",
    "im hintergrund", "heimlich", "dateien geöffnet",
];

pub fn about_program(question: &str) -> bool {
    let q = question.to_lowercase();
    ABOUT_PROGRAM.iter().any(|w| q.contains(w))
}

/// Parts of a path that never go out: places where keys and passwords live, and names that say so.
const SECRET_PLACES: [&str; 8] = ["/.ssh/", "/.gnupg/", "/.password-store/", "/.local/share/keyrings/", "/.mozilla/", "/.config/brave", "/secrets/", "/.aws/"];
const SECRET_WORDS: [&str; 8] = ["passw", "kennwort", "secret", "geheim", "token", "credential", "schlüssel", "private"];
const SECRET_ENDS: [&str; 6] = [".key", ".pem", ".p12", ".kdbx", ".env", ".gpg"];

/// Whether a file must never be named to a provider.
pub fn secret_path(path: &str) -> bool {
    let p = path.to_lowercase();
    let name = p.rsplit('/').next().unwrap_or(&p);
    SECRET_PLACES.iter().any(|s| p.contains(s)) || SECRET_WORDS.iter().any(|w| name.contains(w)) || SECRET_ENDS.iter().any(|e| name.ends_with(e)) || name.starts_with("id_")
}

fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One line per shown element, as `uilist::render` writes it, with the pointed one marked; secret values never.
fn list_lines(e: &Element, depth: usize, pointed: Option<&str>, out: &mut String) {
    if !e.visible {
        return;
    }
    let role = format!("{:?}", e.role).to_lowercase();
    out.push_str(&"  ".repeat(depth + 1));
    out.push_str(&format!("{role} {:?}", e.name));
    if let Some(v) = e.value.as_ref().filter(|_| !e.has(State::Secret)) {
        out.push_str(&format!(" = {v:?}"));
    }
    if !e.state.is_empty() {
        let s: Vec<String> = e.state.iter().map(|s| format!("{s:?}").to_lowercase()).collect();
        out.push_str(&format!(" [{}]", s.join(" ")));
    }
    out.push_str(&format!(" #{}", e.id));
    if pointed == Some(e.id.as_str()) {
        out.push_str("  <- darauf zeigt Georg");
    }
    out.push('\n');
    for c in &e.children {
        list_lines(c, depth + 1, pointed, out);
    }
}

pub fn build(i: &Inputs) -> Question {
    let w = i.window;
    let mut kinds = Vec::new();
    let mut list = format!("Fenster {} von „{}“: „{}“ ({})\n", w.window, w.app, w.title, match w.source {
        uilist::Source::Picture => "nur ein Bild, keine Bedienelemente bekannt",
        _ => "Bedienelemente",
    });
    let mut rows = String::new();
    for e in &w.elements {
        list_lines(e, 0, i.pointed, &mut rows);
    }
    if !rows.is_empty() {
        kinds.push(Kind::Liste);
        if rows.len() > LIST_MAX {
            rows = format!("{}\n  … (gekürzt)\n", cut(&rows, LIST_MAX));
        }
        list.push_str(&rows);
    }
    let extras = access_text(i.access);
    if !extras.is_empty() {
        kinds.push(Kind::Zugriffe);
    }
    if i.picture {
        kinds.push(Kind::Bild);
        list.push_str("\nEin Bild des Fensters liegt bei.\n");
    }
    Question { list, extras, kinds }
}

/// The access log as it may go out: never a line that names a key's or a password's place.
fn access_text(access: Option<&str>) -> String {
    let Some(a) = access.filter(|a| !a.trim().is_empty()) else { return String::new() };
    let mut text = "Was das Programm zuletzt gelesen, geschrieben und verbunden hat:\n".to_string();
    for line in cut(a, ACCESS_MAX).lines().filter(|l| !secret_path(l)) {
        text.push_str(line);
        text.push('\n');
    }
    text
}

/// The answer as Georg reads it (every `[#id]` taken out, an unfinished one at the end too) and the ids it named,
/// each once, in order.
pub fn ids_in(answer: &str) -> (String, Vec<String>) {
    let mut shown = String::new();
    let mut ids: Vec<String> = Vec::new();
    let mut rest = answer;
    while let Some(at) = rest.find("[#") {
        shown.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        match after.find(']') {
            Some(end) if end <= 80 && !after[..end].contains(char::is_whitespace) => {
                let id = after[..end].to_string();
                if !ids.contains(&id) {
                    ids.push(id);
                }
                rest = &after[end + 1..];
            }
            Some(_) => {
                shown.push_str("[#");
                rest = after;
            }
            // Still coming: nothing of it is shown yet.
            None if after.len() <= 80 && !after.contains(char::is_whitespace) => {
                rest = "";
            }
            None => {
                shown.push_str("[#");
                rest = after;
            }
        }
    }
    shown.push_str(rest);
    // A space left before punctuation where a bracket was ("Knopf [#w3/1]." -> "Knopf .").
    let shown = shown.replace(" .", ".").replace(" ,", ",").replace("  ", " ");
    (shown, ids)
}

#[cfg(test)]
#[path = "db_question_tests.rs"]
mod tests;
