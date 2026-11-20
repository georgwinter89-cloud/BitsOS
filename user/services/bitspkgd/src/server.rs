//! The socket: one thread per client (R1: as many as come), its right judged per changing request (`peer`), requests
//! read with a size bound (settingsd's `read_line`), every wait with a deadline (B4: an idle client is closed
//! after `IDLE`, a client that stops reading after `WRITE_WAIT`).

use crate::Ctx;
use crate::ops::{self, REQUEST_MAX};
use settingsd::server::{Line, read_line};
use std::io::{BufReader, Write};
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const IDLE: Duration = Duration::from_secs(600);
pub const WRITE_WAIT: Duration = Duration::from_secs(30);

/// Binds `path` (mode 0666: everyone may ask, the peer check decides what). A socket left by an earlier run is
/// replaced; one another bitspkgd still answers on is not.
pub fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Ok(m) = std::fs::symlink_metadata(path) {
        if !m.file_type().is_socket() {
            return Err(format!("{} ist kein Socket: nicht ersetzt", path.display()));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(format!("{}: ein anderer bitspkgd antwortet dort schon", path.display()));
        }
        std::fs::remove_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let l = UnixListener::bind(path).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(l)
}

pub fn serve(l: UnixListener, cx: Arc<Ctx>) {
    for conn in l.incoming() {
        match conn {
            Ok(s) => {
                let cx = cx.clone();
                if let Err(e) = std::thread::Builder::new().name("pkg-client".into()).spawn(move || client(s, &cx)) {
                    crate::event("pkg.daemon.client_thread", &[("reason", &e.to_string())]);
                }
            }
            Err(e) => {
                crate::event("pkg.daemon.accept_failed", &[("reason", &e.to_string())]);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn client(s: UnixStream, cx: &Arc<Ctx>) {
    let Ok(ps) = s.try_clone() else { return };
    let peer = move || crate::peer::of(&ps);
    if s.set_read_timeout(Some(IDLE)).is_err() || s.set_write_timeout(Some(WRITE_WAIT)).is_err() {
        return;
    }
    let Ok(rs) = s.try_clone() else { return };
    let mut rd = BufReader::new(rs);
    let mut w = s;
    loop {
        let line = match read_line(&mut rd, REQUEST_MAX) {
            Ok(Line::Text(t)) => t,
            Ok(Line::TooLong) => {
                let e = ops::err("bad_request", format!("Anfrage länger als {REQUEST_MAX} Bytes"));
                if writeln!(w, "{}", e.to_text()).is_err() {
                    return;
                }
                continue;
            }
            Ok(Line::End) | Err(_) => return,
        };
        if line.trim().is_empty() {
            continue;
        }
        let mut ok = true;
        ops::handle(cx, &peer, &line, &mut |j| {
            ok = ok && writeln!(w, "{}", j.to_text()).is_ok();
            ok
        });
        if !ok {
            return;
        }
    }
}
