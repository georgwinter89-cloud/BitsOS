//! REWIND: bitscomp's client of `bitssnap serve`'s desktop socket (/run/bitsos/bitssnap.sock, one JSON line each
//! way, user/tools/bitssnap/src/local.rs). As pkg_client: every request in a short thread of its own on its own
//! connection (a restore copies for as long as a folder takes; B4: the frame loop never waits), the answer to
//! the main loop through a calloop channel. A timer asks `list` once a minute, so the strip's "Zurückspulen" knows
//! whether there is anything to rewind to (a still desktop draws nothing for it: only a change does).
//! The same thread runs the restart a confirmed "Alles auf diesen Zeitpunkt" offers (init's power socket, as the
//! sofa mode's).

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::App;

pub const SOCKET: &str = "/run/bitsos/bitssnap.sock";
const LINE_MAX: u64 = 16 << 20;
/// How long a list or a look into a folder may take; a restore may take this long.
pub const SHORT: Duration = Duration::from_secs(30);
pub const LONG: Duration = Duration::from_secs(1800);
/// How often the strip asks whether there are snapshots.
const PROBE_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Ask { id: u64, line: String, wait: Duration },
    /// A restart through init's power socket (`physical`: real input asked for it).
    Power { id: u64, socket: PathBuf, physical: bool },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    /// The answer line (`ok` true or false).
    Reply(u64, Json),
    /// No socket, no answer in time, a broken line: the German reason (first word for the log).
    Failed(u64, String),
    /// Init's answer to the restart: its German line, and whether BitsOS goes down.
    Power(u64, String, bool),
}

pub struct Client {
    socket: PathBuf,
    tx: Sender<Msg>,
}

pub fn start(handle: &LoopHandle<'static, App>, socket: String) -> Result<Client, String> {
    let (tx, rx) = channel::channel::<Msg>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.rewind_msg(m);
            }
        })
        .map_err(|e| e.to_string())?;
    // The first look a moment after the start (bitssnap may come up later than the desktop), then each minute.
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(3)), |_, _, app: &mut App| {
            app.rewind_probe("timer");
            TimeoutAction::ToDuration(PROBE_EVERY)
        })
        .map_err(|e| e.to_string())?;
    Ok(Client { socket: PathBuf::from(socket), tx })
}

impl Client {
    /// Runs `job` in a thread of its own; false: no thread.
    pub fn send(&self, job: Job) -> bool {
        let (socket, tx) = (self.socket.clone(), self.tx.clone());
        std::thread::Builder::new()
            .name("bitscomp-snap".into())
            .spawn(move || {
                let _ = tx.send(run(&socket, &job));
            })
            .is_ok()
    }
}

pub fn run(socket: &std::path::Path, job: &Job) -> Msg {
    match job {
        Job::Ask { id, line, wait } => ask(socket, *id, line, *wait),
        Job::Power { id, socket, physical } => {
            let (text, down) = crate::sofa_io::power_request(socket, "reboot", *physical);
            Msg::Power(*id, text, down)
        }
    }
}

fn ask(socket: &std::path::Path, id: u64, line: &str, wait: Duration) -> Msg {
    let fail = |why: &str| Msg::Failed(id, why.to_string());
    let s = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused) => return fail("absent"),
        Err(_) => return fail("unreachable"),
    };
    // bitssnap may answer "busy" (or turn a stranger away) at accept and close before our line is read: the
    // write then fails, but its answer waits to be read.
    let wrote = s.set_write_timeout(Some(SHORT)).is_ok() && (&s).write_all(format!("{}\n", line.trim()).as_bytes()).is_ok();
    let _ = s.set_read_timeout(Some(if wrote { wait } else { SHORT }));
    let mut buf = Vec::new();
    match BufReader::new(&s).take(LINE_MAX).read_until(b'\n', &mut buf) {
        Ok(_) if buf.ends_with(b"\n") => {}
        Ok(_) if !wrote => return fail("unreachable"),
        Ok(_) => return fail("closed"),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => return fail("late"),
        Err(_) if !wrote => return fail("unreachable"),
        Err(_) => return fail("closed"),
    }
    match Json::parse(String::from_utf8_lossy(&buf).trim()) {
        Ok(j @ Json::Obj(_)) => Msg::Reply(id, j),
        _ => fail("garbled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn dir(what: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bitscomp-snap-{what}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_answer_a_missing_socket_and_a_silent_one() {
        let d = dir("ask");
        let sock = d.join("s.sock");
        assert_eq!(ask(&sock, 1, "{}", SHORT), Msg::Failed(1, "absent".into()));
        let l = UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            for (i, s) in l.incoming().take(3).enumerate() {
                let s = s.unwrap();
                let mut line = String::new();
                BufReader::new(&s).read_line(&mut line).unwrap();
                match i {
                    0 => (&s).write_all(b"{\"ok\":true,\"snapshots\":[]}\n").unwrap(),
                    1 => (&s).write_all(b"no json\n").unwrap(),
                    _ => std::thread::sleep(Duration::from_millis(400)),
                }
            }
        });
        assert_eq!(ask(&sock, 2, r#"{"op":"list"}"#, SHORT), Msg::Reply(2, Json::parse(r#"{"ok":true,"snapshots":[]}"#).unwrap()));
        assert_eq!(ask(&sock, 3, r#"{"op":"list"}"#, SHORT), Msg::Failed(3, "garbled".into()));
        assert_eq!(ask(&sock, 4, r#"{"op":"list"}"#, Duration::from_millis(100)), Msg::Failed(4, "late".into()));
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_answer_given_at_accept_is_read_even_when_our_line_was_not() {
        let d = dir("busy");
        let sock = d.join("s.sock");
        let l = UnixListener::bind(&sock).unwrap();
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            (&s).write_all(b"{\"ok\":false,\"error\":\"busy\"}\n").unwrap();
            drop(s);
        });
        std::thread::sleep(Duration::from_millis(50));
        let m = ask(&sock, 5, r#"{"op":"list"}"#, SHORT);
        assert_eq!(m, Msg::Reply(5, Json::parse(r#"{"ok":false,"error":"busy"}"#).unwrap()));
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }
}
