//! WY-5b: changes from the settings page to settingsd (`set`, `undo`; user/services/settingsd/src/proto.rs). One
//! thread of its own sends them in order, one request per connection with the same 5 s limit as the reader (B4:
//! the frame loop never waits for settingsd). The answer comes back to the main loop as a German line for the page;
//! the new value itself arrives through the watch (`settings`), as every other change does, so the screen follows
//! settingsd and never a guess of it.
//!
//! `undo` in settingsd reverts the newest change of anyone (Claude, MCP, the page). Rückgängig on the page is
//! meant for the page's own changes, so the thread first reads the newest change an undo would revert
//! (`history`, the same rule as settingsd's journal: not an undo, not undone) and undoes it only when its source
//! is the page; else the page names the key and who changed it. A request that waited in the queue longer than
//! the limit (settingsd hung) is dropped, not applied late.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::state::App;

const LIMIT: Duration = Duration::from_secs(5);
/// Shown as the source of the change in settingsd's history.
pub const SOURCE: &str = "einstellungen";

#[derive(Clone, Debug, PartialEq)]
pub enum Req {
    /// `confirmed`: Georg said "Ändern" in the page's dialog (WY-10); settingsd's `confirmed` for a confirm key.
    Set { key: String, value: Json, confirmed: bool },
    /// `confirmed`: the change Georg confirmed the undo of in the dialog. The thread sends the undo only while
    /// the history still names that change (key and seq) as the newest, and the undo names its seq
    /// (`expect_seq`, WY-10/2): a change by another program between that check and the undo is refused by
    /// settingsd (`changed_meanwhile`) instead of being undone.
    Undo { confirmed: Option<Sure> },
}

/// The change an undo dialog was confirmed for.
#[derive(Clone, Debug, PartialEq)]
pub struct Sure {
    pub key: String,
    /// settingsd's journal `seq` (None: the history line had none).
    pub seq: Option<u64>,
}

impl Req {
    pub fn set(key: &str, value: Json) -> Req {
        Req::Set { key: key.into(), value, confirmed: false }
    }

    pub const UNDO: Req = Req::Undo { confirmed: None };
}

/// settingsd wants a confirmation (`needs_confirm`): the request to send after the page's dialog (already
/// confirmed), its key and the value it would set (`Json::Null`: the key's default).
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub req: Req,
    pub key: String,
    pub to: Json,
}

/// settingsd's answer for the page of `pid`.
#[derive(Clone, Debug, PartialEq)]
pub struct Done {
    pub pid: u32,
    pub ok: bool,
    /// The German line for the page (the key's own text is put in by the page).
    pub key: Option<String>,
    pub text: String,
    /// The answer was `needs_confirm`: what the page asks Georg before it sends it again.
    pub ask: Option<Ask>,
}

type Queued = (u32, Req, Instant);

pub struct Writer {
    tx: mpsc::Sender<Queued>,
}

impl Writer {
    /// Queues a change; false when the thread has ended.
    pub fn send(&self, pid: u32, r: Req) -> bool {
        self.tx.send((pid, r, Instant::now())).is_ok()
    }
}

pub fn start(handle: &LoopHandle<'static, App>, socket: String) -> Result<Writer, String> {
    let (done_tx, done_rx) = channel::channel::<Done>();
    handle
        .insert_source(done_rx, |ev, _, app| {
            if let channel::Event::Msg(d) = ev {
                app.page_done(d);
            }
        })
        .map_err(|e| e.to_string())?;
    let (tx, rx) = mpsc::channel::<Queued>();
    std::thread::Builder::new()
        .name("bitscomp-settings-write".into())
        .spawn(move || run(&socket, &rx, &done_tx))
        .map_err(|e| format!("settings writer: {e}"))?;
    Ok(Writer { tx })
}

fn run(socket: &str, rx: &mpsc::Receiver<Queued>, tx: &Sender<Done>) {
    while let Ok((pid, r, queued)) = rx.recv() {
        let waited = queued.elapsed();
        let d = if waited > LIMIT {
            eprintln!("bitscomp: settings change dropped after {} ms in the queue: {}", waited.as_millis(), request_line(&r));
            Done { pid, ok: false, key: key_of(&r), text: "Nicht geändert: der Einstellungsdienst war zu langsam.".into(), ask: None }
        } else {
            one(socket, pid, &r)
        };
        if tx.send(d).is_err() {
            return;
        }
    }
}

fn key_of(r: &Req) -> Option<String> {
    match r {
        Req::Set { key, .. } => Some(key.clone()),
        Req::Undo { .. } => None,
    }
}

fn one(socket: &str, pid: u32, r: &Req) -> Done {
    let mut undoing = None;
    if let Req::Undo { confirmed } = r {
        match newest_undoable(socket) {
            Ok(e) => {
                if let Some(no) = undo_refused(pid, confirmed.as_ref(), e.as_ref()) {
                    return no;
                }
                undoing = e;
            }
            Err(e) => {
                eprintln!("bitscomp: settings history failed: {e}");
                return Done { pid, ok: false, key: None, text: "Der Einstellungsdienst antwortet nicht.".into(), ask: None };
            }
        }
    }
    let line = request_line(r);
    let mut d = match request(socket, &line) {
        Ok(j) => answer(pid, r, &j),
        Err(e) => {
            eprintln!("bitscomp: settings change failed: {e}");
            Done { pid, ok: false, key: None, text: "Der Einstellungsdienst antwortet nicht.".into(), ask: None }
        }
    };
    if !d.ok {
        eprintln!("bitscomp: settingsd refused {line}: {}", d.text);
    }
    // An undo of a confirm key: the page asks first, naming the key and the value it goes back to.
    if let (Some(a), Some(e)) = (d.ask.as_mut(), undoing) {
        a.key = e.key.clone();
        a.to = e.old;
        a.req = Req::Undo { confirmed: Some(Sure { key: e.key.clone(), seq: e.seq }) };
        d.key = Some(e.key);
    }
    d
}

/// Why the page's undo is not sent, given the change `undo n=1` would revert now (`newest`) and the change Georg
/// confirmed the undo of (`confirmed`, None: not confirmed yet).
pub fn undo_refused(pid: u32, confirmed: Option<&Sure>, newest: Option<&Entry>) -> Option<Done> {
    let no = |key: &str, text: String| Some(Done { pid, ok: false, key: Some(key.to_string()), text, ask: None });
    match (newest, confirmed) {
        (Some(e), _) if e.source != SOURCE => {
            eprintln!("bitscomp: page undo refused: the newest change ({} by {:?}) is not the page's", e.key, e.source);
            no(&e.key, foreign(&e.source))
        }
        (Some(e), Some(c)) if c.key != e.key || c.seq != e.seq => {
            eprintln!("bitscomp: confirmed undo of {} seq {:?} not sent: the newest change is now {} seq {:?}", c.key, c.seq, e.key, e.seq);
            no(&e.key, MEANWHILE.into())
        }
        // Nothing to check the confirmation against: it is not sent blind.
        (None, Some(c)) => {
            eprintln!("bitscomp: confirmed undo of {} not sent: the history names no undoable change", c.key);
            no(&c.key, MEANWHILE.into())
        }
        _ => None,
    }
}

/// The page's line when a confirmed undo no longer matches the newest change.
pub const MEANWHILE: &str = "inzwischen neu geändert: nichts rückgängig gemacht.";

/// The page's line when the newest change came from someone else.
pub fn foreign(source: &str) -> String {
    let who = match source {
        "" | "socket" | "cli" => "einem anderen Programm".to_string(),
        s => format!("„{s}“"),
    };
    format!("zuletzt geändert von {who}, nicht auf dieser Seite: nicht rückgängig gemacht.")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    pub source: String,
    /// The value an undo goes back to (`Json::Null`: the default).
    pub old: Json,
    pub seq: Option<u64>,
}

/// The change settingsd's `undo n=1` reverts now (journal.rs `undo_candidates`), read from `history`; `None`
/// when there is none. The window grows until it is found or the history ends (no fixed depth, R1).
fn newest_undoable(socket: &str) -> Result<Option<Entry>, String> {
    let mut n = 32usize;
    loop {
        let line = Json::obj(vec![("op", Json::str("history")), ("n", Json::Num(n as f64))]).to_string();
        let j = request(socket, &line)?;
        let (found, more) = undoable_in(&j, n)?;
        if found.is_some() || !more {
            return Ok(found);
        }
        n = n.saturating_mul(4);
    }
}

/// The newest undoable entry in a `history` answer of `n` asked, and whether older entries may follow.
pub fn undoable_in(j: &Json, n: usize) -> Result<(Option<Entry>, bool), String> {
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(format!("history refused: {j}"));
    }
    let es = j.get("entries").and_then(Json::as_array).ok_or("history without entries")?;
    let s = |e: &Json, k: &str| e.get(k).and_then(Json::as_str).unwrap_or("").to_string();
    let hit = es.iter().find(|e| s(e, "op") != "undo" && e.get("undone").and_then(Json::as_bool) != Some(true));
    let old = |e: &Json| e.get("old").cloned().unwrap_or(Json::Null);
    let seq = |e: &Json| e.get("seq").and_then(Json::as_i64).and_then(|q| u64::try_from(q).ok());
    Ok((hit.map(|e| Entry { key: s(e, "key"), source: s(e, "source"), old: old(e), seq: seq(e) }), es.len() >= n))
}

pub fn request_line(r: &Req) -> String {
    match r {
        Req::Set { key, value, confirmed } => {
            let mut v =
                vec![("op", Json::str("set")), ("key", Json::str(key)), ("value", value.clone()), ("source", Json::str(SOURCE))];
            if *confirmed {
                v.push(("confirmed", Json::Bool(true)));
            }
            Json::obj(v)
        }
        Req::Undo { confirmed } => {
            let mut v = vec![("op", Json::str("undo")), ("n", Json::Num(1.0)), ("source", Json::str(SOURCE))];
            if let Some(c) = confirmed {
                v.push(("confirmed", Json::Bool(true)));
                if let Some(q) = c.seq {
                    v.push(("expect_seq", Json::Num(q as f64)));
                }
            }
            Json::obj(v)
        }
    }
    .to_string()
}

fn request(socket: &str, line: &str) -> Result<Json, String> {
    let mut s = UnixStream::connect(socket).map_err(|e| format!("{socket}: {e}"))?;
    s.set_read_timeout(Some(LIMIT)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(LIMIT)).map_err(|e| e.to_string())?;
    writeln!(s, "{line}").map_err(|e| e.to_string())?;
    let mut out = String::new();
    BufReader::new(s).read_line(&mut out).map_err(|e| e.to_string())?;
    Json::parse(out.trim()).map_err(|e| format!("answer is not JSON: {e}"))
}

/// The page's line for settingsd's answer `j` to `r`.
pub fn answer(pid: u32, r: &Req, j: &Json) -> Done {
    let ok = j.get("ok").and_then(Json::as_bool) == Some(true);
    if !ok {
        let code = j.get("error").and_then(Json::as_str).unwrap_or("");
        let msg = j.get("message").and_then(Json::as_str).unwrap_or("");
        let text = match code {
            "nothing_to_undo" => "Nichts mehr rückgängig zu machen.".to_string(),
            "changed_meanwhile" => MEANWHILE.to_string(),
            "not_undoable" if msg.is_empty() => "Nichts mehr rückgängig zu machen.".to_string(),
            "not_undoable" => format!("Nicht rückgängig zu machen: {msg}"),
            "bad_value" | "bad_request" => format!("Abgelehnt: {msg}"),
            "" => "Der Einstellungsdienst hat nicht geantwortet.".to_string(),
            "unknown_key" => format!("Unbekannte Einstellung: {msg}"),
            "needs_confirm" => format!("Braucht eine Bestätigung: {msg}"),
            _ => format!("Abgelehnt ({code}): {msg}"),
        };
        // Asked again only when this request was not confirmed yet (a confirmed one refused stays refused).
        let ask = match r {
            Req::Set { key, value, confirmed: false } if code == "needs_confirm" => {
                Some(Ask { req: Req::Set { key: key.clone(), value: value.clone(), confirmed: true }, key: key.clone(), to: value.clone() })
            }
            // The thread fills in the key and its old value from the history it read.
            Req::Undo { confirmed: None } if code == "needs_confirm" => Some(Ask { req: Req::UNDO, key: String::new(), to: Json::Null }),
            _ => None,
        };
        let key = match r {
            Req::Undo { confirmed: Some(c) } if code == "changed_meanwhile" => Some(c.key.clone()),
            _ => key_of(r),
        };
        return Done { pid, ok, key, text, ask };
    }
    match r {
        Req::Set { key, .. } => Done { pid, ok, key: Some(key.clone()), text: "Geändert".into(), ask: None },
        Req::Undo { .. } => {
            let key = j
                .get("undone")
                .and_then(Json::as_array)
                .and_then(|a| a.first())
                .and_then(|e| e.get("key"))
                .and_then(Json::as_str)
                .map(String::from);
            if let Req::Undo { confirmed: Some(c) } = r
                && key.as_deref() != Some(c.key.as_str())
            {
                // The window WY-10/2 closes: another program changed something between the check and the undo.
                eprintln!("bitscomp: confirmed undo of {} undid {key:?} instead (changed meanwhile)", c.key);
                return Done { pid, ok, key, text: format!("Achtung: inzwischen geändert, statt {} rückgängig gemacht", c.key), ask: None };
            }
            Done { pid, ok, key, text: "Rückgängig gemacht".into(), ask: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_settingsd_lines() {
        let s = request_line(&Req::set("theme.accent", Json::str("#ffb020")));
        assert_eq!(s, r##"{"op":"set","key":"theme.accent","value":"#ffb020","source":"einstellungen"}"##);
        let b = request_line(&Req::set("theme.layout.statusbar", Json::Bool(false)));
        assert!(b.contains(r#""value":false"#), "{b}");
        assert_eq!(request_line(&Req::UNDO), r#"{"op":"undo","n":1,"source":"einstellungen"}"#);
        let sure = Req::Undo { confirmed: Some(Sure { key: "test.risky".into(), seq: Some(42) }) };
        assert_eq!(request_line(&sure), r#"{"op":"undo","n":1,"source":"einstellungen","confirmed":true,"expect_seq":42}"#);
        let d = answer(1, &sure, &Json::parse(r#"{"ok":false,"error":"changed_meanwhile","message":"x"}"#).unwrap());
        assert!(!d.ok && d.text == MEANWHILE && d.key.as_deref() == Some("test.risky") && d.ask.is_none(), "{d:?}");
    }

    #[test]
    fn the_newest_undoable_change_is_found_in_the_history() {
        let j = Json::parse(
            r##"{"ok":true,"entries":[
              {"seq":5,"key":"theme.mode","op":"undo","source":"einstellungen","undone":false},
              {"seq":4,"key":"theme.mode","op":"set","source":"socket","undone":true},
              {"seq":3,"key":"theme.accent","op":"set","source":"claude","undone":false},
              {"seq":2,"key":"theme.scale","op":"set","source":"einstellungen","undone":false}]}"##,
        )
        .unwrap();
        let (e, more) = undoable_in(&j, 4).unwrap();
        assert_eq!(e, Some(Entry { key: "theme.accent".into(), source: "claude".into(), old: Json::Null, seq: Some(3) }));
        assert!(more);
        let j = Json::parse(r#"{"ok":true,"entries":[{"seq":1,"key":"a","op":"undo","source":"x"}]}"#).unwrap();
        assert_eq!(undoable_in(&j, 32).unwrap(), (None, false));
        assert!(undoable_in(&Json::Null, 32).is_err());
        assert!(foreign("claude").contains("„claude“") && foreign("socket").contains("anderen Programm"));
    }

    #[test]
    fn answers_become_german_lines() {
        let set = Req::set("theme.accent", Json::str("x"));
        let j = Json::parse(r#"{"ok":false,"error":"bad_value","message":"theme.accent: want #rrggbb"}"#).unwrap();
        let d = answer(7, &set, &j);
        assert!(!d.ok && d.text.starts_with("Abgelehnt: ") && d.key.as_deref() == Some("theme.accent"), "{d:?}");
        let j = Json::parse(r#"{"ok":false,"error":"not_undoable","message":""}"#).unwrap();
        assert_eq!(answer(7, &Req::UNDO, &j).text, "Nichts mehr rückgängig zu machen.");
        let j = Json::parse(r##"{"ok":true,"undone":[{"key":"theme.accent","value":"#00e5ff"}]}"##).unwrap();
        let d = answer(7, &Req::UNDO, &j);
        assert!(d.ok && d.key.as_deref() == Some("theme.accent"), "{d:?}");
        assert!(!answer(7, &Req::UNDO, &Json::Null).ok);
    }

    #[test]
    fn only_a_confirmed_request_carries_confirmed_and_needs_confirm_asks_once() {
        let plain = Req::set("test.risky", Json::Num(3.0));
        assert!(!request_line(&plain).contains("confirmed"));
        let sure = Req::Set { key: "test.risky".into(), value: Json::Num(3.0), confirmed: true };
        assert!(request_line(&sure).ends_with(r#","confirmed":true}"#), "{}", request_line(&sure));
        let undo = Req::Undo { confirmed: Some(Sure { key: "test.risky".into(), seq: Some(9) }) };
        assert_eq!(request_line(&undo), r#"{"op":"undo","n":1,"source":"einstellungen","confirmed":true,"expect_seq":9}"#);
        let j = Json::parse(r#"{"ok":false,"error":"needs_confirm","message":"test.risky: ask Georg first"}"#).unwrap();
        let d = answer(7, &plain, &j);
        assert_eq!(d.ask, Some(Ask { req: sure.clone(), key: "test.risky".into(), to: Json::Num(3.0) }), "{d:?}");
        // A confirmed request refused again is not asked again (no loop of dialogs).
        assert_eq!(answer(7, &sure, &j).ask, None);
        assert!(answer(7, &Req::UNDO, &j).ask.is_some() && answer(7, &undo, &j).ask.is_none());
        let other = Json::parse(r#"{"ok":false,"error":"bad_value","message":"x"}"#).unwrap();
        assert_eq!(answer(7, &plain, &other).ask, None);
        let h = Json::parse(r#"{"ok":true,"entries":[{"key":"test.risky","op":"set","source":"einstellungen","old":1}]}"#).unwrap();
        assert_eq!(undoable_in(&h, 32).unwrap().0.map(|e| e.old), Some(Json::Num(1.0)));
    }

    #[test]
    fn a_confirmed_undo_that_undid_another_change_says_so() {
        let sure = Req::Undo { confirmed: Some(Sure { key: "test.risky".into(), seq: Some(9) }) };
        let j = Json::parse(r#"{"ok":true,"undone":[{"key":"test.risky","value":1}]}"#).unwrap();
        assert_eq!(answer(7, &sure, &j).text, "Rückgängig gemacht");
        let j = Json::parse(r#"{"ok":true,"undone":[{"key":"theme.mode","value":"dark"}]}"#).unwrap();
        let d = answer(7, &sure, &j);
        assert!(d.ok && d.text.starts_with("Achtung: inzwischen geändert") && d.key.as_deref() == Some("theme.mode"), "{d:?}");
    }

    #[test]
    fn a_confirmed_undo_is_sent_only_for_the_change_it_was_confirmed_for() {
        let e = |key: &str, seq, source: &str| Entry { key: key.into(), source: source.into(), old: Json::Null, seq };
        let c = Sure { key: "test.risky".into(), seq: Some(9) };
        assert_eq!(undo_refused(7, Some(&c), Some(&e("test.risky", Some(9), SOURCE))), None);
        assert_eq!(undo_refused(7, None, Some(&e("theme.mode", Some(2), SOURCE))), None);
        assert_eq!(undo_refused(7, None, None), None, "settingsd says nothing_to_undo itself");
        // Another change of the same key, another key, an empty history: not sent.
        for newest in [Some(e("test.risky", Some(10), SOURCE)), Some(e("theme.mode", Some(9), SOURCE)), None] {
            let d = undo_refused(7, Some(&c), newest.as_ref()).expect("sent although it was not confirmed for it");
            assert!(!d.ok && d.text == MEANWHILE, "{d:?}");
        }
        let d = undo_refused(7, Some(&c), Some(&e("test.risky", Some(9), "claude"))).unwrap();
        assert!(d.text.contains("„claude“"), "{d:?}");
    }
}
