//! The Unix socket (DESIGN §2.1): JSON lines, `{"id", "op", "args"}` → `{"id", "ok", "result"|"error"}`.
//! Hostile input never panics; a line over 1 MiB is answered with `bad_request` and the connection stays
//! usable. The socket is for Georg's own programs (desktop, CLI, UI): callers here are `Caller::Desktop`.

use crate::api;
use crate::engine::{Caller, Engine};
use serde_json::{Value, json};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Bound on one request line (hostile-input bound, DESIGN §2.1).
pub const MAX_LINE: usize = 1 << 20;

pub enum Line {
    Text(String),
    TooLong,
    Eof,
}

/// Read one line of at most `max` bytes; an over-long line is consumed up to its newline.
pub fn read_line(r: &mut impl BufRead, max: usize) -> io::Result<Line> {
    let mut buf = Vec::new();
    let mut too_long = false;
    loop {
        let avail = match r.fill_buf() {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            return Ok(if buf.is_empty() && !too_long {
                Line::Eof
            } else if too_long {
                Line::TooLong
            } else {
                Line::Text(String::from_utf8_lossy(&buf).into_owned())
            });
        }
        let (chunk, done) = match avail.iter().position(|c| *c == b'\n') {
            Some(p) => (&avail[..p], Some(p + 1)),
            None => (avail, None),
        };
        if !too_long {
            if buf.len() + chunk.len() > max {
                too_long = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        let n = done.unwrap_or(avail.len());
        r.consume(n);
        if done.is_some() {
            return Ok(if too_long { Line::TooLong } else { Line::Text(String::from_utf8_lossy(&buf).into_owned()) });
        }
    }
}

pub fn handle_line(e: &Engine, line: &str) -> Value {
    let req: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return json!({"id": null, "ok": false, "error": {"code": "bad_request", "message": "not JSON"}}),
    };
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let Some(op) = req.get("op").and_then(Value::as_str) else {
        return json!({"id": id, "ok": false, "error": {"code": "bad_request", "message": "op is required"}});
    };
    let args = req.get("args").cloned().unwrap_or(Value::Null);
    let r = match op {
        "ping" => Ok(json!({"pong": true})),
        "reindex" => {
            e.wake_indexer();
            Ok(json!({"queued": true}))
        }
        _ => api::call(e, op, &args, Caller::Desktop),
    };
    match r {
        Ok(v) => json!({"id": id, "ok": true, "result": v}),
        Err(x) => json!({"id": id, "ok": false, "error": {"code": x.code, "message": x.message}}),
    }
}

fn serve_conn(e: Arc<Engine>, s: UnixStream) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(600)));
    let Ok(w) = s.try_clone() else { return };
    let mut w = io::BufWriter::new(w);
    let mut r = BufReader::new(s);
    loop {
        let resp = match read_line(&mut r, MAX_LINE) {
            Ok(Line::Eof) | Err(_) => return,
            Ok(Line::TooLong) => {
                json!({"id": null, "ok": false, "error": {"code": "bad_request", "message": "line longer than 1 MiB"}})
            }
            Ok(Line::Text(l)) if l.trim().is_empty() => continue,
            Ok(Line::Text(l)) => handle_line(&e, &l),
        };
        if writeln!(w, "{resp}").and_then(|_| w.flush()).is_err() {
            return;
        }
    }
}

/// Bind the socket (a stale file from a crashed run is replaced; a live ctxd is not).
pub fn bind(path: &Path) -> Result<UnixListener, String> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(format!("{} is in use by a running ctxd", path.display()));
        }
        let _ = std::fs::remove_file(path);
    }
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    if !parent.is_dir() {
        crate::util::private_dir(parent).map_err(|e| format!("socket folder {}: {e}", parent.display()))?;
    }
    // Bound inside a fresh 0700 folder, narrowed to 0600, then renamed into place: no moment in which another
    // user could connect, whatever the umask and the folder of the final path.
    let stage = parent.join(format!(".ctxd-bind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::DirBuilder::new().mode(0o700).create(&stage).map_err(|e| format!("socket {}: {e}", stage.display()))?;
    let tmp = stage.join("s");
    let r = UnixListener::bind(&tmp).map_err(|e| format!("socket {}: {e}", path.display())).and_then(|l| {
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| format!("socket {}: {e}", path.display()))?;
        Ok(l)
    });
    let _ = std::fs::remove_dir_all(&stage);
    r
}

pub fn serve(e: Arc<Engine>, l: UnixListener) {
    for s in l.incoming() {
        let Ok(s) = s else { continue };
        let e = Arc::clone(&e);
        let _ = std::thread::Builder::new().name("ctxd-sock".into()).spawn(move || serve_conn(e, s));
    }
}

/// Client side (CLI): one request, one answer.
pub fn request(path: &Path, op: &str, args: Value, timeout: Duration) -> Result<Value, String> {
    let s = UnixStream::connect(path).map_err(|e| format!("connect {}: {e}", path.display()))?;
    s.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{}", json!({"id": 1, "op": op, "args": args})).map_err(|e| e.to_string())?;
    let mut r = BufReader::new(s);
    match read_line(&mut r, 256 << 20).map_err(|e| e.to_string())? {
        Line::Text(l) => serde_json::from_str(&l).map_err(|e| e.to_string()),
        _ => Err("no answer".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_lines_are_skipped_not_fatal() {
        let mut data = vec![b'x'; 3000];
        data.push(b'\n');
        data.extend_from_slice(b"{\"op\":\"ping\"}\nlast");
        let mut r = io::Cursor::new(data);
        assert!(matches!(read_line(&mut r, 1000).unwrap(), Line::TooLong));
        assert!(matches!(read_line(&mut r, 1000).unwrap(), Line::Text(t) if t == "{\"op\":\"ping\"}"));
        assert!(matches!(read_line(&mut r, 1000).unwrap(), Line::Text(t) if t == "last"));
        assert!(matches!(read_line(&mut r, 1000).unwrap(), Line::Eof));
    }
}
