//! PK-7a: bitscomp's client of bitspkgd (/run/bitsos/pkg.sock, JSON lines, user/services/bitspkgd/src/ops.rs).
//! Every request runs in a short thread of its own on its own connection, since a reply can take long (a `do`
//! streams progress for minutes; B4: the frame loop never waits for the socket). What comes back goes to the main
//! loop through a calloop channel as `Msg`; the "Programme" page draws only from the model that keeps it
//! (`programme`). Each request has a deadline: no line within it is `Msg::Late` ("Programmverwaltung antwortet
//! nicht"), and a one-shot timer on the main loop says the same where the thread cannot (a connect that hangs).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::App;

pub const SOCKET: &str = "/run/bitsos/pkg.sock";
/// One reply line of bitspkgd is at most this long (its own client's bound).
const LINE_MAX: u64 = 16 << 20;

/// One request: the JSON line, whether it streams `progress` lines before its last one, and its deadlines: `wait`
/// for the whole request, `idle` between two lines of a stream.
#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: u64,
    pub line: String,
    pub stream: bool,
    pub wait: Duration,
    pub idle: Duration,
    /// Not a socket request: the facts of this program folder (`library_facts`), read in the thread.
    pub local: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// A `progress` line of a stream.
    Line(u64, Json),
    /// The last line (`ok` true or false).
    End(u64, Json),
    /// No connection, a broken one, or a line that is no JSON: the German reason.
    Failed(u64, String),
    /// Nothing within the deadline.
    Late(u64),
}

/// The client side the App keeps: where requests go, and the main loop for their deadline timers.
pub struct Client {
    socket: PathBuf,
    tx: Sender<Msg>,
    handle: LoopHandle<'static, App>,
}

pub fn start(handle: &LoopHandle<'static, App>, socket: String) -> Result<Client, String> {
    let (tx, rx) = channel::channel::<Msg>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.prog_msg(m);
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(Client { socket: PathBuf::from(socket), tx, handle: handle.clone() })
}

impl Client {
    /// Sends `job` from a thread of its own; false: no thread (the page says the service does not answer).
    pub fn send(&self, job: Job) -> bool {
        let (socket, tx) = (self.socket.clone(), self.tx.clone());
        let (id, wait) = (job.id, job.wait);
        let spawned = std::thread::Builder::new().name("bitscomp-pkg".into()).spawn(move || {
            let m = run(&socket, &job, &mut |m| tx.send(m).is_ok());
            if let Some(m) = m {
                let _ = tx.send(m);
            }
        });
        if let Err(e) = spawned {
            eprintln!("bitscomp: programme: no thread for request {id}: {e}");
            return false;
        }
        // The main loop's own deadline (a little after the thread's): a request still open then is late.
        let r = self.handle.insert_source(Timer::from_duration(wait + Duration::from_millis(500)), move |_, _, app: &mut App| {
            app.prog_expire(id);
            TimeoutAction::Drop
        });
        if let Err(e) = r {
            eprintln!("bitscomp: programme: no deadline timer for request {id}: {e}");
        }
        true
    }
}

/// One request on its own connection; each stream line goes to `out` (false: the main loop is gone). The last
/// message is returned.
pub fn run(socket: &std::path::Path, job: &Job, out: &mut dyn FnMut(Msg) -> bool) -> Option<Msg> {
    let id = job.id;
    let fail = |why: String| Some(Msg::Failed(id, why));
    if let Some(dir) = &job.local {
        return Some(match crate::library_facts::read(dir) {
            Ok(j) => Msg::End(id, j),
            Err(e) => Msg::Failed(id, e),
        });
    }
    let s = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("bitscomp: programme: connect {}: {e}", socket.display());
            return fail(format!("Programmverwaltung nicht erreichbar ({})", io_de(&e)));
        }
    };
    let start = std::time::Instant::now();
    if s.set_write_timeout(Some(job.wait)).is_err() || (&s).write_all(format!("{}\n", job.line.trim()).as_bytes()).is_err() {
        return fail("Programmverwaltung nahm die Anfrage nicht an".into());
    }
    let Ok(rd) = s.try_clone() else { return fail("Verbindung zur Programmverwaltung kaputt".into()) };
    let mut rd = BufReader::new(rd);
    loop {
        let left = job.wait.saturating_sub(start.elapsed());
        let limit = if job.stream { left.min(job.idle) } else { left };
        if limit.is_zero() || s.set_read_timeout(Some(limit)).is_err() {
            return Some(Msg::Late(id));
        }
        let mut buf = Vec::new();
        match (&mut rd).take(LINE_MAX).read_until(b'\n', &mut buf) {
            Ok(0) => return fail("Programmverwaltung schloss die Verbindung ohne Antwort".into()),
            Ok(_) if buf.last() != Some(&b'\n') && buf.len() as u64 >= LINE_MAX => {
                return fail("eine Antwortzeile der Programmverwaltung ist zu lang".into());
            }
            Ok(_) => {}
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return Some(Msg::Late(id)),
            Err(e) => return fail(format!("Verbindung zur Programmverwaltung abgebrochen ({})", io_de(&e))),
        }
        let text = String::from_utf8_lossy(&buf);
        let j = match Json::parse(text.trim()) {
            Ok(j @ Json::Obj(_)) => j,
            _ => return fail("Programmverwaltung antwortete kein JSON".into()),
        };
        if job.stream && j.get("progress").is_some() && j.get("ok").is_none() {
            if !out(Msg::Line(id, j)) {
                return None;
            }
            continue;
        }
        return Some(Msg::End(id, j));
    }
}

/// A system error in German words (the page shows no English; the raw text is in the log).
fn io_de(e: &std::io::Error) -> String {
    use std::io::ErrorKind as K;
    let t = match e.kind() {
        K::NotFound => "sie läuft nicht",
        K::ConnectionRefused => "sie nimmt keine Verbindung an",
        K::PermissionDenied => "kein Zugriff",
        K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => "die Verbindung brach ab",
        _ => "Systemfehler",
    };
    match e.raw_os_error() {
        Some(n) => format!("{t}, Fehler {n}"),
        None => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn job(stream: bool, wait_ms: u64) -> Job {
        Job { id: 7, line: r#"{"op":"status"}"#.into(), stream, wait: Duration::from_millis(wait_ms), idle: Duration::from_millis(wait_ms), local: None }
    }

    fn serve(lines: &'static [&'static str], pause_ms: u64) -> (tempdir::Dir, std::thread::JoinHandle<()>) {
        let d = tempdir::Dir::new();
        let l = UnixListener::bind(d.0.join("pkg.sock")).unwrap();
        let t = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut rd = BufReader::new(c.try_clone().unwrap());
            let mut req = String::new();
            rd.read_line(&mut req).unwrap();
            for line in lines {
                std::thread::sleep(Duration::from_millis(pause_ms));
                if c.write_all(format!("{line}\n").as_bytes()).is_err() {
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        });
        (d, t)
    }

    mod tempdir {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                let p = std::env::temp_dir().join(format!("bitscomp-pkg-{}-{:?}", std::process::id(), std::thread::current().id()));
                let _ = std::fs::remove_dir_all(&p);
                std::fs::create_dir_all(&p).unwrap();
                Dir(p)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_stream_gives_its_lines_then_the_last() {
        let (d, t) = serve(&[r#"{"progress":"download","done_bytes":5}"#, r#"{"progress":"switch","step":"done"}"#, r#"{"ok":true,"done":true}"#], 10);
        let mut lines = Vec::new();
        let end = run(&d.0.join("pkg.sock"), &job(true, 2000), &mut |m| {
            lines.push(m);
            true
        });
        assert_eq!(lines.len(), 2);
        assert!(matches!(end, Some(Msg::End(7, ref j)) if j.get("done").and_then(Json::as_bool) == Some(true)), "{end:?}");
        t.join().unwrap();
    }

    #[test]
    fn missing_slow_and_broken_services_are_named() {
        let d = tempdir::Dir::new();
        let missing = run(&d.0.join("none.sock"), &job(false, 500), &mut |_| true);
        assert!(matches!(missing, Some(Msg::Failed(7, ref w)) if w.contains("nicht erreichbar")), "{missing:?}");
        drop(d);
        let (d, t) = serve(&[r#"{"ok":true}"#], 600);
        assert_eq!(run(&d.0.join("pkg.sock"), &job(false, 150), &mut |_| true), Some(Msg::Late(7)), "no answer within the deadline");
        t.join().unwrap();
        let (d, t) = serve(&["kein json"], 0);
        assert!(matches!(run(&d.0.join("pkg.sock"), &job(false, 1000), &mut |_| true), Some(Msg::Failed(7, _))));
        t.join().unwrap();
    }
}
