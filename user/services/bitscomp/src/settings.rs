//! WY-5: the settings registry's values for bitscomp (CL-1's settingsd, /run/bitsos/settings.sock, JSON lines,
//! user/services/settingsd/src/proto.rs). A thread of its own talks to it, since its reads block (B4: the frame
//! loop never waits for settingsd): one connection `watch`es every key, a second one `list`s them once (and again
//! after a `lagged` event) and asks for `status` (where settingsd stores, for the session file). What it learns
//! goes to the main loop through a calloop channel; `App::settings_msg` rebuilds the theme and applies it live.
//! No settingsd (nested on CachyOS): the built-in defaults, retried with a back-off.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::page_schema::{self, Spec};
use crate::state::App;

pub const SOCKET: &str = "/run/bitsos/settings.sock";
const BACKOFF_MIN: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(5);
/// A reply that does not come within this ends the connection (the next try starts after the back-off).
const READ_LIMIT: Duration = Duration::from_secs(5);

/// Key -> value as text (ints in decimal, bools as true/false).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Values(BTreeMap<String, String>);

impl Values {
    pub fn text(&self, k: &str) -> Option<&str> {
        self.0.get(k).map(String::as_str)
    }
    pub fn int(&self, k: &str) -> Option<i64> {
        self.text(k)?.parse().ok()
    }
    pub fn flag(&self, k: &str) -> Option<bool> {
        self.text(k)?.parse().ok()
    }
    /// A comma separated list (desktop.favourites), empty parts left out.
    pub fn list(&self, k: &str) -> Option<Vec<String>> {
        Some(self.text(k)?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
    }
    pub fn set(&mut self, k: &str, v: &str) {
        self.0.insert(k.into(), v.into());
    }
    pub fn remove(&mut self, k: &str) {
        self.0.remove(k);
    }
}

fn shown(j: &Json) -> Option<String> {
    match j {
        Json::Str(s) => Some(s.clone()),
        Json::Bool(b) => Some(b.to_string()),
        Json::Num(_) => j.as_i64().map(|n| n.to_string()),
        _ => None,
    }
}

pub enum Msg {
    /// Every key: values, the German text of each (for the address suggestions) and its schema (WY-9: the
    /// settings page's rows).
    All(Values, Vec<(String, String)>, Vec<Spec>),
    Changed(String, Option<String>),
    /// settingsd's storage directory when it is the stick (persistent), else `None`.
    Place(Option<String>),
    /// The connection is gone: the values stay as they were (the next `All` refreshes them).
    Lost,
}

pub fn start(handle: &LoopHandle<'static, App>, socket: String) -> Result<(), String> {
    let (tx, rx) = channel::channel::<Msg>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.settings_msg(m);
            }
        })
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("bitscomp-settings".into())
        .spawn(move || run(&socket, &tx))
        .map(|_| ())
        .map_err(|e| format!("settings thread: {e}"))
}

fn run(socket: &str, tx: &Sender<Msg>) {
    let mut backoff = BACKOFF_MIN;
    let mut last = String::new();
    loop {
        match session(socket, tx) {
            Ok(()) => backoff = BACKOFF_MIN,
            Err(Stop::Gone) => return,
            Err(Stop::Failed(what)) => {
                if what != last {
                    eprintln!("bitscomp: settings {what} (built-in theme until it answers; retrying)");
                    last = what;
                }
            }
        }
        if tx.send(Msg::Lost).is_err() {
            return;
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

enum Stop {
    /// The main loop has ended.
    Gone,
    Failed(String),
}

fn connect(socket: &str) -> Result<UnixStream, Stop> {
    let s = UnixStream::connect(socket).map_err(|e| Stop::Failed(format!("{socket}: {e}")))?;
    s.set_read_timeout(Some(READ_LIMIT)).map_err(|e| Stop::Failed(e.to_string()))?;
    s.set_write_timeout(Some(READ_LIMIT)).map_err(|e| Stop::Failed(e.to_string()))?;
    Ok(s)
}

/// One request on its own connection, one answer.
fn ask(socket: &str, req: &str) -> Result<Json, Stop> {
    let mut s = connect(socket)?;
    writeln!(s, "{req}").map_err(|e| Stop::Failed(e.to_string()))?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| Stop::Failed(e.to_string()))?;
    let j = Json::parse(line.trim()).map_err(|e| Stop::Failed(format!("answer is not JSON: {e}")))?;
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(Stop::Failed(format!("refused: {}", line.trim())));
    }
    Ok(j)
}

fn send(tx: &Sender<Msg>, m: Msg) -> Result<(), Stop> {
    tx.send(m).map_err(|_| Stop::Gone)
}

/// One more request on an open connection; None: the connection is broken (no answer, not JSON).
fn exchange(s: &mut UnixStream, rd: &mut BufReader<UnixStream>, req: &str) -> Option<Json> {
    writeln!(s, "{req}").ok()?;
    let mut line = String::new();
    rd.read_line(&mut line).ok().filter(|n| *n > 0)?;
    Json::parse(line.trim()).ok()
}

/// Every key: the `list`, then (WY-9) one `describe` per key on the same connection for its schema, unless the
/// list carries it. A key settingsd does not describe gets bitscomp's own row or a row that only shows its value.
fn all(socket: &str, tx: &Sender<Msg>) -> Result<(), Stop> {
    let mut s = connect(socket)?;
    let mut rd = BufReader::new(s.try_clone().map_err(|e| Stop::Failed(e.to_string()))?);
    let j = exchange(&mut s, &mut rd, r#"{"op":"list","prefix":""}"#)
        .ok_or_else(|| Stop::Failed("list: no answer".into()))?;
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        return Err(Stop::Failed(format!("list refused: {j}")));
    }
    let mut v = Values::default();
    let mut texts = Vec::new();
    let mut specs = Vec::new();
    let mut open = true;
    let mut undescribed = 0usize;
    for k in j.get("keys").and_then(Json::as_array).unwrap_or(&[]) {
        let Some(key) = k.get("key").and_then(Json::as_str) else { continue };
        if let Some(val) = k.get("value").and_then(shown) {
            v.set(key, &val);
        }
        let text = k.get("text").and_then(Json::as_str).unwrap_or("").to_string();
        let mut spec = page_schema::spec_of(k);
        if spec.is_none() && open {
            let req = Json::obj(vec![("op", Json::str("describe")), ("key", Json::str(key))]).to_string();
            match exchange(&mut s, &mut rd, &req) {
                Some(d) => spec = d.get("key").and_then(page_schema::spec_of).filter(|d| d.key == key),
                None => open = false,
            }
        }
        if spec.is_none() {
            undescribed += 1;
        }
        specs.push(spec.unwrap_or_else(|| page_schema::fallback(key, &text)));
        texts.push((key.to_string(), text));
    }
    if undescribed > 0 {
        eprintln!("bitscomp: settings: {undescribed} of {} keys without a schema (shown, not changed)", specs.len());
    }
    send(tx, Msg::All(v, texts, specs))
}

fn session(socket: &str, tx: &Sender<Msg>) -> Result<(), Stop> {
    // The watch first, so no change between the list and the watch is missed.
    let mut w = connect(socket)?;
    writeln!(w, r#"{{"op":"watch","prefix":""}}"#).map_err(|e| Stop::Failed(e.to_string()))?;
    let mut rd = BufReader::new(w.try_clone().map_err(|e| Stop::Failed(e.to_string()))?);
    let mut line = String::new();
    rd.read_line(&mut line).map_err(|e| Stop::Failed(e.to_string()))?;
    if !line.contains("\"watching\"") {
        return Err(Stop::Failed(format!("watch refused: {}", line.trim())));
    }
    // The watch connection stays quiet while nothing changes: no read limit on it.
    w.set_read_timeout(None).map_err(|e| Stop::Failed(e.to_string()))?;
    all(socket, tx)?;
    let status = ask(socket, r#"{"op":"status"}"#)?;
    let place = (status.get("place").and_then(Json::as_str) == Some("stick"))
        .then(|| status.get("dir").and_then(Json::as_str).map(String::from))
        .flatten();
    send(tx, Msg::Place(place))?;
    loop {
        line.clear();
        match rd.read_line(&mut line) {
            Ok(0) => return Err(Stop::Failed("settingsd closed the watch".into())),
            Err(e) => return Err(Stop::Failed(format!("watch: {e}"))),
            Ok(_) => {}
        }
        match event(&line) {
            Event::Changed(k, v) => send(tx, Msg::Changed(k, v))?,
            // Events were dropped for us: read everything again.
            Event::Lagged => all(socket, tx)?,
            Event::Other => {}
        }
    }
}

#[derive(Debug, PartialEq)]
enum Event {
    /// A key and its new value; a value that is no string, number or bool counts as unset (its default applies).
    Changed(String, Option<String>),
    /// Read everything again (events were dropped, or keys came or went).
    Lagged,
    /// Not JSON, no event, a changed event without a key: skipped.
    Other,
}

/// One line of the watch stream.
fn event(line: &str) -> Event {
    let Ok(j) = Json::parse(line.trim()) else { return Event::Other };
    match j.get("event").and_then(Json::as_str) {
        Some("changed") => match j.get("key").and_then(Json::as_str) {
            Some(k) if !k.is_empty() => Event::Changed(k.into(), j.get("value").and_then(shown)),
            _ => Event::Other,
        },
        // WY-10: keys came or went (a service registered): the rows are read again, as after a lag. settingsd
        // does not send these yet (change request WY-10/1); until then a new key shows after the next list.
        Some("lagged" | "registered" | "unregistered") => Event::Lagged,
        _ => Event::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_read_as_their_type() {
        let mut v = Values::default();
        v.set("theme.scale", "125");
        v.set("theme.layout.statusbar", "false");
        v.set("desktop.favourites", " programm/terminal ,, einstellungen/aussehen");
        assert_eq!(v.int("theme.scale"), Some(125));
        assert_eq!(v.flag("theme.layout.statusbar"), Some(false));
        assert_eq!(v.list("desktop.favourites").unwrap(), ["programm/terminal", "einstellungen/aussehen"]);
        assert_eq!(shown(&Json::Num(7.0)), Some("7".into()));
        assert_eq!(shown(&Json::Bool(true)), Some("true".into()));
        assert_eq!(shown(&Json::Null), None);
    }

    #[test]
    fn garbled_watch_lines_are_skipped() {
        assert_eq!(event("not json at all"), Event::Other);
        assert_eq!(event(""), Event::Other);
        assert_eq!(event("{\"event\":\"changed\"}"), Event::Other, "no key");
        assert_eq!(event("{\"event\":\"changed\",\"key\":7,\"value\":\"x\"}"), Event::Other, "a key that is no string");
        assert_eq!(event("{\"event\":\"changed\",\"key\":\"\",\"value\":\"x\"}"), Event::Other);
        assert_eq!(
            event("{\"event\":\"changed\",\"key\":\"theme.accent\",\"value\":[1,2]}"),
            Event::Changed("theme.accent".into(), None),
            "a value of the wrong type is unset: the default applies"
        );
        assert_eq!(
            event("{\"event\":\"changed\",\"key\":\"theme.mode\",\"value\":\"light\"}\n"),
            Event::Changed("theme.mode".into(), Some("light".into()))
        );
        assert_eq!(event("{\"event\":\"lagged\"}"), Event::Lagged);
        assert_eq!(event("{\"event\":\"registered\",\"owner\":\"testsvc\"}"), Event::Lagged);
        assert_eq!(event("{\"event\":\"whatever\"}"), Event::Other);
        assert_eq!(event("[\"event\"]"), Event::Other);
    }

    /// A fake settingsd that answers the watch, lists junk next to a good key, sends garbage, a lag, a good change,
    /// and closes: the client keeps what is good, reads everything again after the lag, and ends with an error
    /// (the caller retries after the back-off) instead of panicking.
    #[test]
    fn a_hostile_settingsd_stream_is_survived() {
        use std::os::unix::net::UnixListener;
        use smithay::reexports::calloop::EventLoop;
        let dir = std::env::temp_dir().join(format!("bitscomp-settings-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let l = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let list = r#"{"ok":true,"keys":[{"key":"theme.mode","value":"light","text":"Hell"},{"value":"no key"},{"key":"theme.scale","value":{"x":1}},7]}"#;
            let (mut watch, _) = l.accept().unwrap();
            let mut req = String::new();
            BufReader::new(watch.try_clone().unwrap()).read_line(&mut req).unwrap();
            writeln!(watch, r#"{{"ok":true,"watching":""}}"#).unwrap();
            let mut lists = 0;
            for answer in [list, r#"{"ok":true,"place":"stick","dir":"/stick/settings"}"#] {
                let (mut c, _) = l.accept().unwrap();
                let mut rd = BufReader::new(c.try_clone().unwrap());
                rd.read_line(&mut req).unwrap();
                writeln!(c, "{answer}").unwrap();
                if lists == 0 {
                    // WY-9: the describes on the list's connection: a good one, then garbage (the rest unasked).
                    let describe = r#"{"ok":true,"key":{"key":"theme.mode","schema":{"type":"choice","choices":["dark","light","dim"]},"default":"dark","text":{"de":"Modus"}}}"#;
                    for answer in [describe, "\u{0}garbage"] {
                        req.clear();
                        rd.read_line(&mut req).unwrap();
                        assert!(req.contains(r#""op":"describe""#), "{req}");
                        writeln!(c, "{answer}").unwrap();
                    }
                }
                lists += 1;
            }
            for line in ["garbage", "{\"event\":\"changed\"}", "\u{0}\u{1}", "{\"event\":\"lagged\"}"] {
                writeln!(watch, "{line}").unwrap();
            }
            // The lag: the client lists again on a new connection.
            let (mut c, _) = l.accept().unwrap();
            BufReader::new(c.try_clone().unwrap()).read_line(&mut req).unwrap();
            writeln!(c, "{list}").unwrap();
            writeln!(watch, r##"{{"event":"changed","key":"theme.accent","value":"#ffb020"}}"##).unwrap();
            drop(watch);
            lists
        });
        let mut ev: EventLoop<Vec<Msg>> = EventLoop::try_new().unwrap();
        let (tx, rx) = channel::channel::<Msg>();
        ev.handle()
            .insert_source(rx, |e, _, got: &mut Vec<Msg>| {
                if let channel::Event::Msg(m) = e {
                    got.push(m);
                }
            })
            .unwrap();
        let end = session(path.to_str().unwrap(), &tx);
        assert!(matches!(end, Err(Stop::Failed(ref w)) if w.contains("closed")), "the closed watch ends the session");
        assert_eq!(server.join().unwrap(), 2);
        let mut got = Vec::new();
        ev.dispatch(Some(Duration::from_millis(100)), &mut got).unwrap();
        let alls: Vec<&Values> = got.iter().filter_map(|m| if let Msg::All(v, _, _) = m { Some(v) } else { None }).collect();
        assert_eq!(alls.len(), 2, "listed at start and again after the lag");
        assert_eq!(alls[0].text("theme.mode"), Some("light"));
        assert_eq!(alls[0].text("theme.scale"), None, "an object value is left out");
        let specs: Vec<&Vec<Spec>> = got.iter().filter_map(|m| if let Msg::All(_, _, s) = m { Some(s) } else { None }).collect();
        let keys: Vec<&str> = specs[0].iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["theme.mode", "theme.scale"]);
        assert_eq!(specs[0][0].ty, page_schema::Ty::Choice(vec!["dark".into(), "light".into(), "dim".into()]), "described");
        assert_eq!(specs[0][1], page_schema::fallback("theme.scale", ""), "garbage: bitscomp's own row");
        assert!(got.iter().any(|m| matches!(m, Msg::Place(Some(d)) if d == "/stick/settings")));
        assert!(got.iter().any(|m| matches!(m, Msg::Changed(k, Some(v)) if k == "theme.accent" && v == "#ffb020")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn describes_that_run_long_break_off_or_time_out_are_survived() {
        use crate::layout::Rect;
        use crate::page::{Inputs, layout};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("bitscomp-describe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let l = UnixListener::bind(&path).unwrap();
        let many: Vec<String> = (0..5000).map(|i| format!("\"v{i}\"")).collect();
        let long = format!(r#"{{"ok":true,"key":{{"key":"zz.a","schema":{{"type":"choice","choices":[{}]}}}}}}"#, many.join(","));
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut rd = BufReader::new(c.try_clone().unwrap());
            let mut req = String::new();
            rd.read_line(&mut req).unwrap();
            writeln!(c, r#"{{"ok":true,"keys":[{{"key":"zz.a"}},{{"key":"zz.b","text":"B"}},{{"key":"zz.c"}}]}}"#).unwrap();
            let mut describes = 0;
            req.clear();
            rd.read_line(&mut req).unwrap();
            describes += req.contains(r#""op":"describe""#) as usize;
            writeln!(c, "{long}").unwrap();
            req.clear();
            rd.read_line(&mut req).unwrap();
            describes += req.contains(r#""op":"describe""#) as usize;
            // The answer breaks off mid-line and the connection ends: zz.c is never asked.
            write!(c, r#"{{"ok":true,"key":{{"key":"zz.b","sch"#).unwrap();
            drop((c, rd));
            describes
        });
        let mut ev: smithay::reexports::calloop::EventLoop<Vec<Msg>> = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        let (tx, rx) = channel::channel::<Msg>();
        ev.handle().insert_source(rx, |e, _, got: &mut Vec<Msg>| if let channel::Event::Msg(m) = e { got.push(m) }).unwrap();
        assert!(all(path.to_str().unwrap(), &tx).is_ok());
        assert_eq!(server.join().unwrap(), 2, "two describes, none after the break");
        let mut got = Vec::new();
        ev.dispatch(Some(Duration::from_millis(100)), &mut got).unwrap();
        let Some(Msg::All(_, _, specs)) = got.pop() else { panic!("no list") };
        assert!(matches!(&specs[0].ty, page_schema::Ty::Choice(c) if c.len() == 5000));
        assert_eq!(specs[1], page_schema::fallback("zz.b", "B"), "broken off: bitscomp's own row");
        assert_eq!(specs[2], page_schema::fallback("zz.c", ""), "unasked after the break");
        let m = page_schema::model(&specs);
        for c in [Rect { x: 0, y: 0, w: 1500, h: 900 }, Rect { x: 0, y: 0, w: 520, h: 900 }] {
            let l = layout(c, &m, &Inputs { favourites: 0, scroll: 0 });
            assert!(l.parts.len() < 1000, "only the visible pills are parts: {}", l.parts.len());
        }
        let _ = std::fs::remove_dir_all(&dir);
        // A describe that is never answered: the read limit ends it, no answer, no panic.
        let (mut a, _b) = UnixStream::pair().unwrap();
        a.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let mut rd = BufReader::new(a.try_clone().unwrap());
        assert!(exchange(&mut a, &mut rd, r#"{"op":"describe","key":"zz.a"}"#).is_none());
    }
}
