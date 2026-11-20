//! WY-5b: changes from the settings page to settingsd (`set`, `undo`, and `register` for the per-workspace
//! Absicht keys of the Bühne, CR DS-3-4; user/services/settingsd/src/proto.rs). One
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
    /// CR DS-3-4: bitscomp's own keys (owner bitscomp, in the shared `desktop` namespace): the Absicht headline of
    /// each workspace, as (key, workspace name).
    Register { keys: Vec<(String, String)> },
    /// GM-6a: bitscomp's own typed keys (the sofa mode's of sofa-mode.md §6), each a full settingsd definition.
    Defs { keys: Vec<Json> },
    /// CL-5: a provider's key Georg typed on the page "KI". It goes as a confirmed change (INT-CL3T: a secret is
    /// written by its owner's program or confirmed) and only while `whilst` (address, kind, place as the page
    /// showed them when he began to type) still holds: settingsd's `while`, as `assist --key` sends it.
    Secret { key: String, value: Hidden, whilst: Vec<(String, Json)> },
    /// CL-5: a key back to its default (a stored provider key removed); `confirmed` as for `Set`.
    Reset { key: String, confirmed: bool },
    /// CL-5: a change Georg confirmed in a dialog of the page "KI", sent only while `whilst` still holds
    /// (settingsd's `while`): the list of providers as the dialog was built from it, so his yes never removes
    /// more than the dialog named.
    Guarded { key: String, value: Json, whilst: Vec<(String, Json)> },
    /// EINS-B (PLAN K8): bitscomp's keys below `prefix` unregistered and their stored values dropped.
    Unregister { prefix: String },
}

/// A secret on its way to settingsd: no Debug output, no log line and no page text ever shows it, and its bytes
/// are overwritten when it is dropped.
#[derive(Clone, PartialEq)]
pub struct Hidden(String);

impl Hidden {
    pub fn new(s: String) -> Hidden {
        Hidden(s)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn is_in(&self, text: &str) -> bool {
        !self.0.is_empty() && text.contains(self.0.as_str())
    }
}

impl std::fmt::Debug for Hidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

impl Drop for Hidden {
    fn drop(&mut self) {
        wipe(std::mem::take(&mut self.0));
    }
}

/// Overwrites a text that held a secret before its memory goes back.
pub fn wipe(s: String) {
    let mut b = s.into_bytes();
    b.fill(0);
    std::hint::black_box(&b);
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

    /// A writer whose queue a test reads (no thread, no socket).
    #[cfg(test)]
    pub fn for_test() -> (Writer, mpsc::Receiver<(u32, Req, Instant)>) {
        let (tx, rx) = mpsc::channel();
        (Writer { tx }, rx)
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
            eprintln!("bitscomp: settings change dropped after {} ms in the queue: {}", waited.as_millis(), logged_line(&r));
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
        Req::Set { key, .. } | Req::Secret { key, .. } | Req::Reset { key, .. } | Req::Guarded { key, .. } => Some(key.clone()),
        Req::Undo { .. } => None,
        Req::Register { keys } => keys.first().map(|(k, _)| k.clone()),
        Req::Defs { keys } => keys.first().and_then(|d| d.get("key")).and_then(Json::as_str).map(String::from),
        Req::Unregister { prefix } => Some(prefix.clone()),
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
            Done { pid, ok: false, key: key_of(r).filter(|_| matches!(r, Req::Secret { .. })), text: "Der Einstellungsdienst antwortet nicht.".into(), ask: None }
        }
    };
    if matches!(r, Req::Secret { .. }) {
        wipe(line);
    }
    if !d.ok {
        eprintln!("bitscomp: settingsd refused {}: {}", logged_line(r), d.text);
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

/// The answer line of a `Reset` settingsd took.
pub const DONE_RESET: &str = "Zurückgesetzt";

/// The page's line when a confirmed undo no longer matches the newest change.
pub const MEANWHILE: &str = "inzwischen neu geändert: nichts rückgängig gemacht.";

/// CL-5: the page's line when the list of providers is no longer the one a removal's dialog was built from.
/// A confirmed change whose value changed elsewhere while the dialog was open.
pub const GUARD_MOVED: &str = "Der Wert hat sich inzwischen anderswo geändert: nichts geändert, bitte neu ansehen.";
pub const LIST_MOVED: &str = "Die Liste der Anbieter hat sich geändert: nichts entfernt, bitte neu ansehen.";

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

/// GM-6d: the secrets bitscomp writes (the sofa mode's Wi-Fi password): their value never reaches a log line.
pub const SECRETS: [&str; 1] = ["net.wifi.psk"];

/// CL-5: a key whose value never reaches a log line: the listed ones and every provider's key
/// (`ai.provider.<id>.key`), whatever the request that carries it.
pub fn is_secret(key: &str) -> bool {
    SECRETS.contains(&key) || key.strip_prefix("ai.provider.").and_then(|r| r.strip_suffix(".key")).is_some_and(|id| !id.is_empty() && !id.contains('.'))
}

/// The request line as the log shows it: a secret's value is `***`.
pub fn logged_line(r: &Req) -> String {
    match r {
        Req::Set { key, confirmed, .. } if is_secret(key) => {
            request_line(&Req::Set { key: key.clone(), value: Json::str("***"), confirmed: *confirmed })
        }
        Req::Secret { key, whilst, .. } => request_line(&Req::Secret { key: key.clone(), value: Hidden::new("***".into()), whilst: whilst.clone() }),
        _ => request_line(r),
    }
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
        Req::Register { keys } => {
            let defs = keys.iter().map(|(k, name)| absicht_def(k, name)).collect();
            Json::obj(vec![("op", Json::str("register")), ("owner", Json::str(OWNER)), ("keys", Json::Arr(defs))])
        }
        Req::Defs { keys } => Json::obj(vec![("op", Json::str("register")), ("owner", Json::str(OWNER)), ("keys", Json::Arr(keys.clone()))]),
        Req::Unregister { prefix } => Json::obj(vec![("op", Json::str("unregister")), ("owner", Json::str(OWNER)), ("prefix", Json::str(prefix)), ("forget", Json::Bool(true))]),
        Req::Secret { key, value, whilst } => Json::obj(vec![
            ("op", Json::str("set")),
            ("key", Json::str(key)),
            ("value", Json::str(&value.0)),
            ("source", Json::str(SOURCE)),
            ("confirmed", Json::Bool(true)),
            ("while", Json::obj(whilst.iter().map(|(k, v)| (k.as_str(), v.clone())).collect())),
        ]),
        Req::Reset { key, confirmed } => {
            let mut v = vec![("op", Json::str("reset")), ("key", Json::str(key)), ("source", Json::str(SOURCE))];
            if *confirmed {
                v.push(("confirmed", Json::Bool(true)));
            }
            Json::obj(v)
        }
        Req::Guarded { key, value, whilst } => Json::obj(vec![
            ("op", Json::str("set")),
            ("key", Json::str(key)),
            ("value", value.clone()),
            ("source", Json::str(SOURCE)),
            ("confirmed", Json::Bool(true)),
            ("while", Json::obj(whilst.iter().map(|(k, v)| (k.as_str(), v.clone())).collect())),
        ]),
    }
    .to_string()
}

/// The owner bitscomp registers its keys as (settingsd checks it against the key's applier).
pub const OWNER: &str = "bitscomp";

/// One Absicht key: free text up to 400 bytes (about 200 characters with umlauts), empty by default (no headline).
fn absicht_def(key: &str, name: &str) -> Json {
    let text = Json::obj(vec![
        ("de", Json::str(&format!("Absicht des Arbeitsraums „{name}“ (Überschrift auf der Bühne; leer: keine)"))),
        ("en", Json::str(&format!("Intent of workspace \"{name}\" (headline on the stage; empty: none)"))),
    ]);
    Json::obj(vec![
        ("key", Json::str(key)),
        ("schema", Json::obj(vec![("type", Json::str("text")), ("max_bytes", Json::Num(400.0))])),
        ("default", Json::str("")),
        ("text", text),
        ("restart", Json::str("none")),
        ("safety", Json::str("normal")),
    ])
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
        // A refusal never repeats a secret it was sent (settingsd does not; this holds whatever it answers).
        let msg = match r {
            Req::Secret { value, .. } if value.is_in(msg) => "der Wert wurde abgelehnt",
            _ => msg,
        };
        let text = match code {
            "nothing_to_undo" => "Nichts mehr rückgängig zu machen.".to_string(),
            "changed_meanwhile" if matches!(r, Req::Secret { .. }) => {
                "Nicht gespeichert: Adresse, Art oder Ort haben sich geändert, während du getippt hast. Bitte ansehen und neu eingeben.".to_string()
            }
            "changed_meanwhile" if matches!(r, Req::Guarded { key, .. } if key == crate::ki::LIST) => LIST_MOVED.to_string(),
            "changed_meanwhile" if matches!(r, Req::Guarded { .. }) => GUARD_MOVED.to_string(),
            // A stored key binds address, kind and place: settingsd's sentence names console commands, the
            // page says the same with its own buttons.
            "key_bound" => key_of(r).as_deref().and_then(crate::ki::provider_of).map_or_else(|| msg.to_string(), |(id, _)| crate::ki_kinds::key_bound(id)),
            // CL-5: settingsd's own German sentence, as it is (INT-CL-4: this bitscomp's yes does not count, it
            // does not hold the screen).
            "not_confirmer" => msg.to_string(),
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
            // A reset back to a default that gives the assistant more freedom (settingsd's leash): asked like a set.
            Req::Reset { key, confirmed: false } if code == "needs_confirm" => {
                Some(Ask { req: Req::Reset { key: key.clone(), confirmed: true }, key: key.clone(), to: Json::Null })
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
        Req::Set { key, .. } | Req::Guarded { key, .. } => Done { pid, ok, key: Some(key.clone()), text: "Geändert".into(), ask: None },
        Req::Secret { key, .. } => Done { pid, ok, key: Some(key.clone()), text: "Gespeichert".into(), ask: None },
        Req::Reset { key, .. } => Done { pid, ok, key: Some(key.clone()), text: DONE_RESET.into(), ask: None },
        Req::Register { .. } | Req::Defs { .. } => Done { pid, ok, key: key_of(r), text: "Angemeldet".into(), ask: None },
        Req::Unregister { .. } => Done { pid, ok, key: key_of(r), text: "Abgemeldet".into(), ask: None },
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
        let reg = Req::Register { keys: vec![("desktop.absicht.arbeitsraum-1".into(), "Arbeitsraum 1".into())] };
        let j = Json::parse(&request_line(&reg)).unwrap();
        assert_eq!(j.get("op").and_then(Json::as_str), Some("register"));
        assert_eq!(j.get("owner").and_then(Json::as_str), Some("bitscomp"));
        let k = &j.get("keys").and_then(Json::as_array).unwrap()[0];
        assert_eq!(k.get("key").and_then(Json::as_str), Some("desktop.absicht.arbeitsraum-1"));
        assert_eq!(k.get("default").and_then(Json::as_str), Some(""));
        let de = k.get("text").and_then(|t| t.get("de")).and_then(Json::as_str).unwrap();
        assert!(de.contains("„Arbeitsraum 1“"), "{de}");
        let sure = Req::Undo { confirmed: Some(Sure { key: "test.risky".into(), seq: Some(42) }) };
        assert_eq!(request_line(&sure), r#"{"op":"undo","n":1,"source":"einstellungen","confirmed":true,"expect_seq":42}"#);
        let d = answer(1, &sure, &Json::parse(r#"{"ok":false,"error":"changed_meanwhile","message":"x"}"#).unwrap());
        assert!(!d.ok && d.text == MEANWHILE && d.key.as_deref() == Some("test.risky") && d.ask.is_none(), "{d:?}");
    }

    #[test]
    fn a_secret_never_reaches_a_log_line_and_other_values_do() {
        let psk = logged_line(&Req::set("net.wifi.psk", Json::str("geheim")));
        assert!(!psk.contains("geheim") && psk.contains(r#""value":"***""#) && psk.contains("net.wifi.psk"), "{psk}");
        let sure = logged_line(&Req::Set { key: "net.wifi.psk".into(), value: Json::str("geheim"), confirmed: true });
        assert!(!sure.contains("geheim") && sure.contains(r#""confirmed":true"#), "{sure}");
        let ssid = logged_line(&Req::set("net.wifi.ssid", Json::str("Flur")));
        assert!(ssid.contains(r#""value":"Flur""#), "{ssid}");
        // The request settingsd gets carries the value itself.
        assert!(request_line(&Req::set("net.wifi.psk", Json::str("geheim"))).contains("geheim"));
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
        // WY-SET fixer: a reset the leash refuses is asked too (to the default), a confirmed one is not.
        let reset = Req::Reset { key: "ai.helper.write.allow".into(), confirmed: false };
        let sure_reset = Req::Reset { key: "ai.helper.write.allow".into(), confirmed: true };
        assert_eq!(answer(7, &reset, &j).ask, Some(Ask { req: sure_reset.clone(), key: "ai.helper.write.allow".into(), to: Json::Null }));
        assert_eq!(answer(7, &sure_reset, &j).ask, None);
        assert!(request_line(&sure_reset).ends_with(r#","confirmed":true}"#));
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
