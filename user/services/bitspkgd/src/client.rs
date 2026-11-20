//! `bitspkgd call JSON`: one request, its lines printed as they come, until the line with `ok` (tests, root's
//! console). Exit status 0 for `"ok":true`.

use settingsd::json::{self, Json};
use settingsd::server::{Line, read_line};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

const LINE_MAX: usize = 16 << 20;

pub fn call(socket: &Path, req: &str, wait: Duration, each: &mut dyn FnMut(&str)) -> Result<bool, String> {
    let s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let end = Instant::now() + wait;
    s.set_write_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
    (&s).write_all(format!("{}\n", req.trim()).as_bytes()).map_err(|e| e.to_string())?;
    let mut rd = BufReader::new(s.try_clone().map_err(|e| e.to_string())?);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(format!("keine Antwort nach {} s", wait.as_secs()));
        }
        s.set_read_timeout(Some(left)).map_err(|e| e.to_string())?;
        match read_line(&mut rd, LINE_MAX) {
            Ok(Line::Text(t)) => {
                each(&t);
                if let Ok(j) = json::parse(&t)
                    && let Some(ok) = j.get("ok").and_then(Json::as_bool)
                {
                    return Ok(ok);
                }
            }
            Ok(Line::TooLong) => return Err(format!("eine Antwortzeile länger als {LINE_MAX} Bytes")),
            Ok(Line::End) => return Err("bitspkgd schloss die Verbindung ohne Antwort".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
}
