//! DB3, EINS-B: the popup's messages back to the main loop (calloop channel, B4: nothing here blocks it) and the
//! one thing it asks a local service for: the access log of the window's program (C5), read on a thread and sent
//! along only with a question about what a program did. The answers come from `assist serve` (PLAN K1).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use smithay::reexports::calloop::channel::Sender;

/// C5: the access log's queries (DB2; missing socket: no access log).
pub const ACCESS_SOCKET: &str = "/run/bitsos/access.sock";
const SERVICE_WAIT: Duration = Duration::from_secs(3);
const LINE_MAX: u64 = 4 << 20;

#[derive(Debug, PartialEq)]
pub enum Msg {
    /// The access log of the window's program (None: none, or nothing in it).
    Access(Option<String>),
    /// DB5b: the Automodus's pause before step `n` of the task is over.
    ActGo(u64),
    /// DB5b: read the window again after a step (a page shows a stored change once settingsd answered).
    ActRead,
    /// The window's picture, made into a PNG off the main loop (`db_shot`).
    Shot(Option<crate::db_shot::Shot>),
    /// EINS-B: a picture a desktop tool asked for (`foto`, `sehen`), for its call.
    DeskShot(String, Option<crate::db_shot::Shot>),
    /// EINS-B: what `bits papierkorb liste` printed, for the view that asked.
    Trash(String),
}

/// One request line to a local service's socket (`{id, op, args}`), its `result`.
pub fn service(path: &Path, op: &str, args: Value) -> Result<Value, String> {
    let mut s = UnixStream::connect(path).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(SERVICE_WAIT)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(SERVICE_WAIT)).map_err(|e| e.to_string())?;
    writeln!(s, "{}", json!({"id": 1, "op": op, "args": args})).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s.take(LINE_MAX)).read_line(&mut line).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
    // ctxd answers {ok, result} or {ok:false, error}; bitsaccess the result itself or {error}.
    if v.get("ok").and_then(Value::as_bool) == Some(false) || (v.get("error").is_some() && v.get("result").is_none()) {
        return Err(v.get("error").map(|e| e.to_string()).unwrap_or_default());
    }
    Ok(v.get("result").cloned().unwrap_or(v))
}

/// Reads the access log of window `pid`'s program on a thread; it comes back as `Msg::Access`.
pub fn prep(app: String, pid: i32, generation: u64, out: Sender<(u64, Msg)>) {
    let _ = std::thread::Builder::new().name("bitscomp-db-prep".into()).spawn(move || {
        // Read here, on this machine; it goes into a question only when that is about what a program did.
        let _ = out.send((generation, Msg::Access(access(&access_socket(), &app, pid))));
    });
}

/// C5 `summary` (DB2's bitsaccess) of the program of window `pid` (its file, else `app`) over the last ten minutes,
/// as lines: what it started, the folders it read and wrote (never one under a private root, never one that looks
/// like keys or passwords), the hosts it talked to, the devices and rights. None: no access log or nothing in it.
pub fn access(path: &Path, app: &str, pid: i32) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok().map(|p| p.to_string_lossy().into_owned()).filter(|_| pid > 0);
    let r = service(path, "summary", json!({"since_s": 600, "app": exe.as_deref().unwrap_or(app)})).ok()?;
    access_lines(&r)
}

fn access_lines(r: &Value) -> Option<String> {
    let n = |v: &Value, k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let t = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").chars().take(160).collect::<String>();
    let mut out = String::new();
    for p in r.get("programs").and_then(Value::as_array).into_iter().flatten().take(4) {
        out.push_str(&format!("Programm {} ({}): {}× gestartet\n", t(p, "name"), t(p, "app"), n(p, "started")));
        for f in p.get("files").and_then(Value::as_array).into_iter().flatten().take(20) {
            let folder = t(f, "folder");
            if f.get("private").and_then(Value::as_bool) == Some(true) || crate::db_question::secret_path(&format!("{folder}/")) {
                continue;
            }
            out.push_str(&format!("- Ordner {folder}: {} gelesen, {} geschrieben, {} angelegt, {} gelöscht\n", n(f, "read"), n(f, "write"), n(f, "create"), n(f, "delete")));
        }
        for c in p.get("net").and_then(Value::as_array).into_iter().flatten().take(20) {
            let host = Some(t(c, "words")).filter(|w| !w.is_empty()).unwrap_or_else(|| t(c, "host"));
            out.push_str(&format!("- Verbindung zu {host} Port {} ({}): {} Bytes gesendet, {} empfangen\n", n(c, "port"), t(c, "port_words"), n(c, "tx"), n(c, "rx")));
        }
        for d in p.get("devices").and_then(Value::as_array).into_iter().flatten().take(10) {
            out.push_str(&format!("- Gerät: {} ({}×)\n", t(d, "label"), n(d, "count")));
        }
        for x in p.get("rights").and_then(Value::as_array).into_iter().flatten().take(10) {
            out.push_str(&format!("- Rechte: {} {}\n", t(x, "kind"), t(x, "what")));
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

fn access_socket() -> std::path::PathBuf {
    ACCESS_SOCKET.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_access_log_in_lines_without_private_folders() {
        let r: Value = serde_json::from_str(r#"{"since":1,"now":2,"on":true,"lost":0,"programs":[{"app":"/usr/lib/brave/brave","name":"brave","system":false,"started":2,"pids":[7],"files":[{"folder":"/data/home/1000/Dokumente","private":false,"read":3,"write":1,"create":0,"delete":0,"rename":0,"last":2,"new":false},{"folder":"/data/home/1000/Tagebuch","private":true,"read":9,"write":0,"create":0,"delete":0,"rename":0,"last":2,"new":false},{"folder":"/data/home/1000/.ssh","private":false,"read":1,"write":0,"create":0,"delete":0,"rename":0,"last":2,"new":false}],"net":[{"host":"203.0.113.9","words":"search.brave.com","proto":"tcp","port":443,"port_words":"https","connect":1,"tx":900,"rx":12000}],"devices":[{"class":"mic","label":"Mikrofon","count":1}],"rights":[]}]}"#).unwrap();
        let l = access_lines(&r).unwrap();
        assert!(l.contains("Programm brave (/usr/lib/brave/brave): 2× gestartet") && l.contains("Ordner /data/home/1000/Dokumente: 3 gelesen") && l.contains("search.brave.com Port 443") && l.contains("Gerät: Mikrofon"), "{l}");
        assert!(!l.contains("Tagebuch") && !l.contains(".ssh"), "never a private root, never keys: {l}");
        assert_eq!(access_lines(&serde_json::json!({"programs": []})), None);
    }

    #[test]
    fn a_missing_access_log_is_no_access_log() {
        assert_eq!(access(Path::new("/nonexistent/access.sock"), "brave", 0), None);
    }
}
