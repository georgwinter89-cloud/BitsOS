//! DB5b, EINS-B: the rules of one acting task (the desktop tool `handeln`), without the desktop. The model
//! proposes one step at a time; BitsOS decides here whether the step is refused (hard limits that hold at every
//! level and for MCP agents too, F1), asked (at `ask` each step; at every level a step that cannot be undone, F2)
//! or run. A small model once ran a destructive restore without asking: the question
//! is BitsOS's job, never the model's.

use std::time::{Duration, Instant};

use serde_json::Value;
use uilist::{Action, Element, Role, Source, State, Window};

use crate::assist_level::Level;

/// Steps a task may propose at most (refused ones count: a model in a loop ends too).
pub const STEP_BUDGET: u32 = 30;
/// How long a task may take from Georg's request to its last step.
pub const TIME_LIMIT: Duration = Duration::from_secs(10 * 60);
/// The Automodus waits this long before each step: Georg sees it coming and Esc stops it before it runs.
pub const AUTO_PAUSE: Duration = Duration::from_millis(700);
/// The most text one `type` step writes.
pub const MAX_TEXT: usize = 2000;
/// EINGABE: a one-line field that holds at most this much may be typed into in the Automodus without a question
/// (an address bar with its URL, a search box).
pub const SHORT: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub element: String,
    pub action: Action,
    pub text: Option<String>,
    /// The element's name as the model read it: another name now means the screen changed.
    pub name: String,
    /// The model's own word whether the step cannot be undone (None: it did not say).
    pub flag: Option<bool>,
    pub why: String,
}

/// The tool's arguments (`durchblick_handeln`); Err is the German reason for the model.
pub fn parse(args: &Value) -> Result<Step, String> {
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty());
    let element = s("element").map(uilist::clean_id).filter(|e| !e.is_empty()).ok_or("Es fehlt, welches Element (element).")?.chars().take(120).collect();
    let word = s("aktion").ok_or("Es fehlt, was getan werden soll (aktion).")?;
    let action = Action::of_word(word).ok_or_else(|| format!("Die Aktion „{word}“ gibt es nicht."))?;
    let text = args.get("text").and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
    if text.as_ref().is_some_and(|t| t.chars().count() > MAX_TEXT) {
        return Err(format!("Der Text ist zu lang: höchstens {MAX_TEXT} Zeichen in einem Schritt."));
    }
    Ok(Step {
        element,
        action,
        text,
        name: s("name").unwrap_or("").chars().take(200).collect(),
        flag: args.get("unumkehrbar").and_then(Value::as_bool),
        why: s("warum").unwrap_or("").chars().take(300).collect(),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Risk {
    Normal,
    /// Cannot be undone (or BitsOS is not sure); the German words say why.
    Final(&'static str),
}

const BUYS: &str = "kauft oder bezahlt etwas";
const SENDS: &str = "schickt etwas ab";
const PUBLISHES: &str = "veröffentlicht etwas";
const DELETES: &str = "löscht oder überschreibt etwas";
const TERMS: &str = "stimmt Bedingungen zu";
const ACCOUNT: &str = "ändert etwas an einem Konto";
const ASKS_ITSELF: &str = "das Programm fragt dafür selbst noch einmal nach";
const ENTER: &str = "der Zeilenumbruch kann ein Formular absenden";
const MODEL: &str = "das Modell sagt, es lässt sich nicht rückgängig machen";
const SIGNS: &str = "unterschreibt etwas";
pub const UNSURE: &str = "Der Assistent ist nicht sicher, ob es sich rückgängig machen lässt";

/// Word beginnings (German and English), lowercase; a token that starts with one matches.
const STEMS: &[(&str, &str)] = &[
    ("kauf", BUYS), ("bestell", BUYS), ("bezahl", BUYS), ("zahlung", BUYS), ("kasse", BUYS), ("checkout", BUYS),
    ("purchas", BUYS), ("abonnier", BUYS), ("abonnem", BUYS), ("subscrib", BUYS), ("buy", BUYS), ("pay", BUYS), ("überweis", BUYS),
    ("transfer", BUYS), ("spend", BUYS), ("donat", BUYS), ("kostenpflicht", BUYS),
    ("unterschreib", SIGNS), ("unterzeichn", SIGNS), ("signier", SIGNS),
    ("send", SENDS), ("absend", SENDS), ("abschick", SENDS), ("verschick", SENDS), ("übermittel", SENDS), ("submit", SENDS),
    ("veröffentlich", PUBLISHES), ("publish", PUBLISHES), ("poste", PUBLISHES), ("share", PUBLISHES), ("upload", PUBLISHES),
    ("hochlad", PUBLISHES), ("tweet", PUBLISHES),
    ("lösch", DELETES), ("loesch", DELETES), ("entfern", DELETES), ("delet", DELETES), ("remov", DELETES), ("vernicht", DELETES),
    ("formatier", DELETES), ("deinstall", DELETES), ("uninstall", DELETES), ("überschreib", DELETES), ("overwrit", DELETES),
    ("ersetz", DELETES), ("verwerf", DELETES), ("discard", DELETES), ("erase", DELETES), ("wipe", DELETES), ("zurücksetz", DELETES),
    ("reset", DELETES), ("wiederherstell", DELETES), ("restor", DELETES), ("kündig", DELETES), ("vergess", DELETES),
    ("forget", DELETES), ("purge", DELETES), ("leeren", DELETES), ("gelösch", DELETES), ("geloesch", DELETES), ("überschrieb", DELETES),
    ("zurückgesetz", DELETES), ("gesend", SENDS), ("abgeschick", SENDS), ("gekauf", BUYS), ("hochgelad", PUBLISHES),
    ("akzeptier", TERMS), ("accept", TERMS), ("zustimm", TERMS), ("einverstanden", TERMS), ("agree", TERMS), ("annehm", TERMS),
];
/// Whole words only (short ones that begin too many harmless words).
const WORDS: &[(&str, &str)] = &[
    ("order", BUYS), ("zahlen", BUYS), ("buchen", BUYS), ("book", BUYS), ("abo", BUYS), ("abos", BUYS), ("post", PUBLISHES),
    ("teilen", PUBLISHES), ("clear", DELETES), ("empty", DELETES), ("sign", SIGNS),
];
/// Words of a window or element about an account.
const ACCOUNTS: &[&str] = &["konto", "account", "benutzerkonto", "profil", "anmeldedaten", "zugangsdaten"];
/// Words of an element (not of a whole window: a mail program's title says "E-Mail") about an account's log-in.
const LOGINS: &[&str] = &["passwort", "password", "kennwort", "email"];
/// "Nicht speichern", "Don't Save", "Ohne Speichern schließen": the work since the last save is gone.
const NOT: &[&str] = &["nicht", "ohne", "without", "dont", "not"];
/// A press that only says yes to whatever its window asks.
const YES: &[&str] = &["ok", "ja", "yes", "weiter", "fortfahren", "continue", "fertig", "done", "bestätigen", "confirm", "übernehmen", "apply", "next"];

fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_string).collect()
}

/// The first thing the words do that cannot be undone, if any.
pub fn final_words(s: &str) -> Option<&'static str> {
    all_final_words(s).into_iter().next()
}

/// Everything the words do that cannot be undone, in their order (a window's "senden" must not hide its "kaufen").
pub fn all_final_words(s: &str) -> Vec<&'static str> {
    let t = tokens(s);
    let not = t.iter().enumerate().any(|(i, w)| NOT.contains(&w.as_str()) || (w == "don" && t.get(i + 1).is_some_and(|n| n == "t")));
    let mut out = Vec::new();
    if not && t.iter().any(|w| w.starts_with("speicher") || w.starts_with("sav")) {
        out.push(DELETES);
    }
    out.extend(t.iter().filter_map(|w| STEMS.iter().find(|(p, _)| w.starts_with(p)).or_else(|| WORDS.iter().find(|(p, _)| w == p)).map(|(_, why)| *why)));
    out
}

fn about_account(s: &str) -> bool {
    tokens(s).iter().any(|w| ACCOUNTS.iter().any(|a| w.starts_with(a)))
}

fn about_login(s: &str) -> bool {
    s.to_lowercase().contains("e-mail") || tokens(s).iter().any(|w| LOGINS.iter().any(|a| w.starts_with(a)))
}

/// Whether typing into `e` may lose something that cannot be had back. bitscomp's own fields: ui_act empties a
/// field it is not editing yet. A foreign program's field (EINGABE, Georg 14.11.): empty, or one line with at most
/// `SHORT` characters (an address bar's URL, a search box) is harmless; longer text and a field of several lines
/// with content are asked; a field whose text cannot be read may hold anything.
fn overwrites(w: &Window, e: &Element) -> Option<&'static str> {
    if w.source != Source::Atspi {
        return match e.value.as_deref() {
            Some(v) if e.role == Role::Field && !v.is_empty() && !e.has(State::Focused) => Some(DELETES),
            _ => None,
        };
    }
    if !matches!(e.role, Role::Field | Role::Choice) {
        return None;
    }
    match e.value.as_deref() {
        None => Some(UNSURE),
        Some("") => None,
        Some(v) if e.has(State::Singleline) && v.chars().count() <= SHORT => None,
        Some(_) => Some(DELETES),
    }
}

/// What the window says around the element: its title and its texts and headings.
fn window_words(w: &Window) -> String {
    let mut out = w.title.clone();
    for e in w.walk().into_iter().filter(|e| matches!(e.role, Role::Text | Role::Heading)) {
        out.push(' ');
        out.push_str(&e.name);
        if let Some(v) = &e.value {
            out.push(' ');
            out.extend(v.chars().take(300));
        }
    }
    out
}

/// Name words of an address bar or a search box (the second signal after the program's own word, `State::Search`).
const SEARCH_NAMES: &[&str] = &["suche", "suchen", "suchleiste", "suchfeld", "search", "omnibox"];

/// EINGABE2: an address bar or a search box, recognised narrowly. A field: the program says so (`State::Search`:
/// its role or semantics) and it has one line, or its name says so and it has one line or the program's word
/// too. An editable combobox (`Choice`: a chat's or a mail's composer with suggestions is one as well) only with
/// both the name and (the program's word or one line): Google's box is a combobox of several lines named "Suche".
pub fn search_field(e: &Element) -> bool {
    let named = tokens(&e.name).iter().any(|t| SEARCH_NAMES.iter().any(|s| t.starts_with(s)));
    let (said, one) = (e.has(State::Search), e.has(State::Singleline) && !e.has(State::Multiline));
    match e.role {
        Role::Field => (said && one) || (named && (one || said)),
        Role::Choice => named && (said || one),
        _ => false,
    }
}

/// EINGABE2 (Georg 14.11.): Enter in the browser's address bar and in a search box runs in the Automodus; in every
/// other field it sends something; a window that speaks of paying, buying, signing, deleting or an account makes
/// even a search box's Enter a step that is asked.
fn submit_risk(w: &Window, e: &Element) -> Risk {
    let grave = |why: &&'static str| [BUYS, DELETES, SIGNS].contains(why);
    let words = window_words(w);
    if let Some(why) = all_final_words(&words).into_iter().chain(all_final_words(&e.name)).find(grave) {
        return Risk::Final(why);
    }
    if about_account(&words) || about_account(&e.name) {
        return Risk::Final(ACCOUNT);
    }
    if search_field(e) { Risk::Normal } else { Risk::Final(SENDS) }
}

/// Whether the step cannot be undone, by BitsOS's own reading; unsure counts as cannot. A step is judged by what
/// its input does: on bitscomp's own windows focus and choose are the same click as press (ui_act).
pub fn classify(w: &Window, e: &Element, s: &Step) -> Risk {
    if s.flag == Some(true) {
        return Risk::Final(MODEL);
    }
    let action = match s.action {
        Action::Focus | Action::Choose if w.source != Source::Atspi => Action::Press,
        a => a,
    };
    if action == Action::Submit {
        return submit_risk(w, e);
    }
    if action == Action::Type {
        if s.text.as_deref().is_some_and(|t| t.contains(['\n', '\r'])) {
            return Risk::Final(ENTER);
        }
        return overwrites(w, e).map_or(Risk::Normal, Risk::Final);
    }
    if !matches!(action, Action::Press | Action::Choose | Action::Toggle) {
        return Risk::Normal;
    }
    // An option chosen by its words (atspid picks it by the text) is judged by those words too.
    let chosen = s.text.as_deref().filter(|_| s.action == Action::Choose);
    if let Some(why) = final_words(&e.name).or_else(|| e.value.as_deref().and_then(final_words)).or_else(|| chosen.and_then(final_words)) {
        return Risk::Final(why);
    }
    if about_account(&e.name) || about_account(&w.title) || about_login(&e.name) {
        return Risk::Final(ACCOUNT);
    }
    let press = action != Action::Toggle;
    if press && e.confirm {
        return Risk::Final(ASKS_ITSELF);
    }
    // "OK" in a dialog that speaks of deleting deletes.
    if press && tokens(&e.name).iter().any(|t| YES.contains(&t.as_str())) {
        if let Some(why) = final_words(&window_words(w)) {
            return Risk::Final(why);
        }
    }
    if action == Action::Press && (e.name.trim().is_empty() || s.flag.is_none()) {
        return Risk::Final(UNSURE);
    }
    Risk::Normal
}

/// Names of fields the assistant never types into: passwords, keys, codes, payment data, log-in names.
const SECRET_STEMS: &[&str] = &[
    "passwort", "password", "kennwort", "passphrase", "passcode", "sicherheitscode", "bestätigungscode", "einmalcode", "schlüssel",
    "token", "geheim", "secret", "benutzername", "username", "anmeldename", "kartennummer", "kreditkarte", "creditcard", "cardnumber",
    "karteninhaber", "cvc", "cvv", "prüfnummer", "prüfziffer", "ablaufdatum", "expir", "iban", "kontonummer", "bankleitzahl",
];
const SECRET_WORDS: &[&str] = &["pin", "tan", "otp", "bic", "blz", "login", "card", "key"];

pub fn secret_field(e: &Element) -> bool {
    let t = tokens(&e.name);
    e.has(State::Secret) || t.iter().any(|w| SECRET_STEMS.iter().any(|s| w.contains(s)) || SECRET_WORDS.contains(&w.as_str()))
}

/// A card number (13 to 19 digits that pass the Luhn check) or an IBAN somewhere in the text.
pub fn payment_text(t: &str) -> bool {
    let compact: String = t.chars().filter(|c| !matches!(c, ' ' | '-' | '.')).collect();
    let luhn = |d: &str| {
        let sum: u32 = d.bytes().rev().enumerate().map(|(i, b)| {
            let n = (b - b'0') as u32;
            if i % 2 == 1 { if n * 2 > 9 { n * 2 - 9 } else { n * 2 } } else { n }
        }).sum();
        sum % 10 == 0
    };
    let digits = compact.split(|c: char| !c.is_ascii_digit()).any(|run| (13..=19).contains(&run.len()) && luhn(run));
    let up: Vec<u8> = compact.to_uppercase().bytes().collect();
    // An IBAN: country letters, check digits, 11 to 30 more, and the whole passes the mod-97 check.
    let iban = (0..up.len()).any(|i| {
        let b = &up[i..];
        b.len() >= 15 && b[0].is_ascii_uppercase() && b[1].is_ascii_uppercase() && b[2].is_ascii_digit() && b[3].is_ascii_digit() && (15..=b.len().min(34)).any(|n| b[..n].iter().all(u8::is_ascii_alphanumeric) && mod97(&b[..n]))
    });
    digits || iban
}

fn mod97(b: &[u8]) -> bool {
    let mut r: u32 = 0;
    for &c in b[4..].iter().chain(&b[..4]) {
        let v = if c.is_ascii_digit() { (c - b'0') as u32 } else { (c - b'A') as u32 + 10 };
        r = if v >= 10 { (r * 100 + v) % 97 } else { (r * 10 + v) % 97 };
    }
    r == 1
}

/// What never happens, in any mode and for any agent: Err is the German reason.
pub fn hard_limit(e: &Element, a: Action, text: Option<&str>) -> Result<(), String> {
    if e.answers {
        return Err(format!("„{}“ beantwortet eine Rückfrage; das bestätigt Georg selbst. Es wurde nichts getan.", e.name));
    }
    if e.confirm && !matches!(e.role, Role::Button | Role::Link) {
        return Err(format!("„{}“ ist eine Einstellung, die vorher nachfragt; sie ändert nur Georg selbst. Es wurde nichts getan.", e.name));
    }
    if a == Action::Type && secret_field(e) {
        return Err(format!("In „{}“ tippt der Assistent nie: Passwörter, Schlüssel, Codes und Zahlungsdaten gibt nur Georg ein.", e.name));
    }
    // EINGABE2: Enter only in a field, never in one of a log-in, a secret or payment data.
    if a == Action::Submit && !matches!(e.role, Role::Field | Role::Choice) {
        return Err(format!("„{}“ ist kein Feld; abschicken geht nur in einem Feld. Es wurde nichts getan.", e.name));
    }
    if a == Action::Submit && (secret_field(e) || about_login(&e.name) || e.has(State::Secret)) {
        return Err(format!("In „{}“ schickt der Assistent nie etwas ab: Anmeldungen, Passwörter, Codes und Zahlungsdaten bleiben Georgs.", e.name));
    }
    // A key of the on-screen keyboard while it types a WLAN key is secret too: pressing it types the secret. Only
    // going into a secret field (Georg then types himself) and scrolling stay.
    if e.has(State::Secret) && !(a == Action::Scroll || (a == Action::Focus && e.role == Role::Field)) {
        return Err(format!("„{}“ gehört zu einer geheimen Eingabe; Passwörter, Schlüssel und Codes gibt nur Georg ein. Es wurde nichts getan.", e.name));
    }
    // No Enter: a line break in a one-line field would send its form.
    if a == Action::Type && e.has(State::Singleline) && text.is_some_and(|t| t.contains(['\n', '\r'])) {
        return Err(format!("„{}“ hat nur eine Zeile; einen Zeilenumbruch (Enter) tippt der Assistent dort nie. Es wurde nichts getan.", e.name));
    }
    if a == Action::Type && text.is_some_and(payment_text) {
        return Err("Der Text sieht nach Zahlungsdaten aus (eine Kartennummer oder IBAN); die gibt der Assistent nie ein.".into());
    }
    Ok(())
}

/// Whether an element above `id` in `w` is a settings row that asks first (its "mehr", its "Löschen").
fn under_confirm(w: &Window, id: &str) -> bool {
    fn go(v: &[Element], id: &str, above: bool) -> Option<bool> {
        v.iter().find_map(|e| if e.id == id { Some(above) } else { go(&e.children, id, above || e.confirm) })
    }
    go(&w.elements, id, false).unwrap_or(false)
}

/// The hard limits with the element's place: a part of a settings row that asks first is that setting too.
pub fn limits(w: &Window, e: &Element, a: Action, text: Option<&str>) -> Result<(), String> {
    if under_confirm(w, &e.id) {
        return Err(format!("„{}“ gehört zu einer Einstellung, die vorher nachfragt; sie ändert nur Georg selbst. Es wurde nichts getan.", e.name));
    }
    hard_limit(e, a, text)
}

/// The step in plain German, for the panel and the journal's reader.
pub fn describe(e: &Element, s: &Step) -> String {
    let n = if e.name.trim().is_empty() { "das Element ohne Namen".to_string() } else { format!("„{}“", e.name.trim()) };
    match s.action {
        Action::Press => format!("{n} drücken"),
        Action::Choose => format!("{n} auswählen"),
        Action::Toggle => format!("{n} {}", if e.has(State::Checked) { "ausschalten" } else { "einschalten" }),
        Action::Focus => format!("zu {n} gehen"),
        Action::Scroll => format!("{n} rollen"),
        Action::Submit => format!("in {n} Enter drücken (abschicken)"),
        Action::Type if secret_field(e) => format!("in {n} tippen"),
        Action::Type => format!("in {n} „{}“ tippen", s.text.as_deref().unwrap_or("").chars().take(80).collect::<String>()),
    }
}

/// What BitsOS does with a proposed step.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    Run,
    Ask,
    Refuse(String),
    /// The task ends: budget, time, a screen the plan did not expect.
    Stop(String),
}

#[derive(Clone, Debug)]
pub struct Task {
    pub n: u64,
    pub started: Instant,
    /// Steps proposed so far (refused ones count: a model in a loop ends too).
    pub steps: u32,
    pub done: u32,
    /// The question came from Georg's own hand: only then may the Automodus run steps unasked.
    pub by_georg: bool,
    /// The program of the first step (the journal's).
    pub app: String,
    pub stopped: Option<String>,
}

impl Task {
    /// The decision for step `s` on element `e` of window `w` (both as they are now; None: gone). `own` says the
    /// element is the desktop's that only Georg answers (a question's buttons, the assistant's own views);
    /// `hidden` that the window's program is on the never-see list. Any other window of any program may be
    /// acted in (EINS-B); the level decides whether a step asks (F2: one that cannot be undone always does).
    #[allow(clippy::too_many_arguments)]
    pub fn decide(&mut self, now: Instant, w: Option<&Window>, e: Option<&Element>, own: bool, hidden: bool, s: &Step, level: Level) -> (Next, Risk) {
        if let Some(why) = &self.stopped {
            return (Next::Stop(why.clone()), Risk::Normal);
        }
        self.steps += 1;
        if self.steps > STEP_BUDGET {
            return (Next::Stop(format!("Der Assistent hat schon {STEP_BUDGET} Schritte vorgeschlagen; mehr macht er für eine Frage nicht.")), Risk::Normal);
        }
        if now.duration_since(self.started) > TIME_LIMIT {
            return (Next::Stop(format!("Die Aufgabe läuft schon über {} Minuten; der Assistent hört auf.", TIME_LIMIT.as_secs() / 60)), Risk::Normal);
        }
        if level == Level::Answer {
            return (Next::Refuse(crate::db_act_app::ANSWER_ONLY.into()), Risk::Normal);
        }
        let Some(w) = w else {
            return (Next::Stop("Das Fenster gibt es nicht mehr.".into()), Risk::Normal);
        };
        if hidden {
            return (Next::Refuse(format!("{}. Es wurde nichts getan.", crate::assist_level::NOT_SEEN)), Risk::Normal);
        }
        if own {
            return (Next::Refuse("Fragen von BitsOS und die Knöpfe des Assistenten selbst bedient nur Georg. Es wurde nichts getan.".into()), Risk::Normal);
        }
        let Some(e) = e else {
            return (Next::Stop(format!("Das Element {} ist nicht mehr da: der Bildschirm sieht anders aus als geplant.", s.element)), Risk::Normal);
        };
        let (want, have) = (s.name.trim().to_lowercase(), e.name.trim().to_lowercase());
        if !want.is_empty() && !have.contains(&want) && !want.contains(&have) {
            return (Next::Stop(format!("Dort steht jetzt „{}“ statt „{}“: der Bildschirm sieht anders aus als geplant.", e.name, s.name)), Risk::Normal);
        }
        if let Err(why) = limits(w, e, s.action, s.text.as_deref()) {
            return (Next::Refuse(why), Risk::Normal);
        }
        let risk = classify(w, e, s);
        let unasked = self.by_georg && level == Level::Auto && risk == Risk::Normal;
        (if unasked { Next::Run } else { Next::Ask }, risk)
    }
}

#[cfg(test)]
#[path = "db_act_tests.rs"]
mod tests;
