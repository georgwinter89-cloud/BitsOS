//! The services `bits` wraps and how each is reached. Every path can be moved by an environment variable (the host
//! tests and scenarios run the services in a folder of their own); BITS.md lists them.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

use crate::out::{Fail, R};

pub fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => PathBuf::from(format!("/run/user/{}", usys::getuid().unwrap_or(1000))),
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// bitscomp's control socket: `BITS_CTL`, else the one `WAYLAND_DISPLAY` names, else the only `*.ctl` socket in the
/// runtime folder (assist's rule: a program started by the desktop does not always carry WAYLAND_DISPLAY).
pub fn ctl_path() -> R<PathBuf> {
    if let Some(p) = env_path("BITS_CTL") {
        return Ok(p);
    }
    let run = runtime_dir();
    if let Ok(d) = std::env::var("WAYLAND_DISPLAY") {
        let p = run.join(format!("{d}.ctl"));
        if p.exists() {
            return Ok(p);
        }
    }
    let mut found: Vec<PathBuf> = std::fs::read_dir(&run)
        .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ctl")).collect())
        .unwrap_or_default();
    found.sort();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(Fail::unreachable(format!("Die Oberfläche läuft nicht (kein Steuer-Anschluss in {}).", run.display()))),
        _ => Err(Fail::unreachable("Es laufen mehrere Oberflächen; BITS_CTL sagt, welche gemeint ist.")),
    }
}

pub fn settings_path() -> PathBuf {
    env_path("SETTINGS_SOCKET").unwrap_or_else(|| PathBuf::from("/run/bitsos/settings.sock"))
}

pub fn ctxd_path() -> PathBuf {
    env_path("BITS_CTXD").unwrap_or_else(|| runtime_dir().join("ctxd.sock"))
}

pub fn pkg_path() -> PathBuf {
    env_path("BITS_PKG").unwrap_or_else(|| PathBuf::from("/run/bitsos/pkg.sock"))
}

fn connect(path: &PathBuf, who: &str, wait: Duration) -> R<UnixStream> {
    let s = UnixStream::connect(path).map_err(|e| Fail::unreachable(format!("{who} antwortet nicht ({}: {e}).", path.display())))?;
    let _ = s.set_read_timeout(Some(wait));
    let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
    Ok(s)
}

/// The longest a control socket answer is waited for (a foreign program's list can take its provider's time).
pub const CTL_WAIT: Duration = Duration::from_secs(40);

/// One line to bitscomp's control socket, its whole answer back (`error: ...` and `refused: ...` as Fail).
pub fn ctl(line: &str) -> R<String> {
    let raw = ctl_raw(line)?;
    if let Some(e) = raw.strip_prefix("error: ") {
        return Err(Fail::failed(e.trim()));
    }
    if let Some(e) = raw.strip_prefix("refused: ") {
        return Err(Fail::refused(e.trim()));
    }
    Ok(raw)
}

pub fn ctl_raw(line: &str) -> R<String> {
    if line.contains('\n') {
        return Err(Fail::usage("Ein Befehl an die Oberfläche ist eine Zeile."));
    }
    let mut s = connect(&ctl_path()?, "Die Oberfläche", CTL_WAIT)?;
    s.write_all(format!("{line}\n").as_bytes()).map_err(|e| Fail::unreachable(format!("Die Oberfläche nimmt nichts an: {e}")))?;
    let mut out = String::new();
    s.read_to_string(&mut out).map_err(|e| Fail::unreachable(format!("Die Oberfläche antwortet nicht: {e}")))?;
    if out.is_empty() {
        return Err(Fail::unreachable("Die Oberfläche hat ohne Antwort aufgelegt."));
    }
    Ok(out.trim_end_matches('\n').to_string())
}

/// A request to a JSON-lines service (settingsd, bitspkgd): the first answer line with `ok`; lines before it
/// (bitspkgd's `progress`) go to `progress`.
pub fn json_line(path: &PathBuf, who: &str, req: &Value, wait: Duration, mut progress: impl FnMut(&Value)) -> R<Value> {
    let s = connect(path, who, wait)?;
    let mut w = s.try_clone().map_err(|e| Fail::unreachable(e.to_string()))?;
    writeln!(w, "{req}").map_err(|e| Fail::unreachable(format!("{who} nimmt nichts an: {e}")))?;
    let mut rd = BufReader::new(s);
    loop {
        let mut line = String::new();
        match rd.read_line(&mut line) {
            Ok(0) => return Err(Fail::unreachable(format!("{who} hat ohne Antwort aufgelegt."))),
            Ok(_) => {}
            Err(e) => return Err(Fail::unreachable(format!("{who} antwortet nicht: {e}"))),
        }
        let v: Value = serde_json::from_str(line.trim()).map_err(|e| Fail::failed(format!("{who} antwortet unverständlich: {e}")))?;
        if v.get("ok").is_some() {
            return Ok(v);
        }
        progress(&v);
    }
}

/// The service's own refusal as a Fail (`{"ok":false,"error","message"}`), the answer otherwise.
pub fn ok_or_fail(v: Value) -> R<Value> {
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(v);
    }
    let msg = v.get("message").and_then(Value::as_str).unwrap_or("abgelehnt").to_string();
    let code = v.get("error").and_then(Value::as_str).unwrap_or("");
    Err(match code {
        "read_only" | "not_owner" | "needs_confirm" | "not_confirmer" | "refused_r8" | "locked" => Fail::refused(msg),
        "unknown_key" | "not_found" => Fail::new("not_found", msg),
        _ => Fail::failed(msg),
    })
}

/// ctxd: `{"id","op","args"}` -> `{"id","ok","result"|"error":{code,message}}`.
pub fn ctxd(op: &str, args: Value) -> R<Value> {
    let v = json_line(&ctxd_path(), "Der Kontextdienst", &json!({"id": 1, "op": op, "args": args}), Duration::from_secs(30), |_| {})?;
    if v.get("ok").and_then(Value::as_bool) == Some(true) {
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    let e = v.get("error").cloned().unwrap_or(Value::Null);
    let msg = e.get("message").and_then(Value::as_str).unwrap_or("abgelehnt").to_string();
    Err(match e.get("code").and_then(Value::as_str) {
        Some("not_found") => Fail::new("not_found", msg),
        Some("bad_request") => Fail::usage(msg),
        _ => Fail::failed(msg),
    })
}
