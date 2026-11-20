//! DB4: bitscomp's client of ctxd, the one local memory (`$XDG_RUNTIME_DIR/ctxd.sock`, JSON lines `{id, op,
//! args}`, C4 of the Durchblick contracts). What Georg does goes there fire-and-forget: one worker thread with a
//! bounded queue owns the connection, a full queue or a missing ctxd drops the event (B4: the desktop never
//! waits for its memory). Questions (rules, suggestions, the learned list, forgetting) come back through a
//! calloop channel; a timer asks again every half minute.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::time::Duration;

use mcpreg::Json;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};

use crate::state::App;

const QUEUE: usize = 256;
const WAIT: Duration = Duration::from_secs(2);
const LINE_MAX: u64 = 4 << 20;
pub const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq)]
pub enum Tag {
    Rules,
    Suggest,
    Learned,
    /// A forget asked on page `pid` (0: from `ctl`).
    Forget(u32, String),
}

pub enum Job {
    Event(String),
    Ask(Tag, String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Msg {
    Reply(Tag, Json),
    Failed(Tag, String),
}

pub struct Client {
    tx: SyncSender<Job>,
    pub dropped: std::cell::Cell<u64>,
}

/// The socket of this user's ctxd.
pub fn socket() -> PathBuf {
    let run = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map_or_else(|| PathBuf::from(format!("/run/user/{}", crate::drop_root::euid())), PathBuf::from);
    run.join("ctxd.sock")
}

pub fn line(op: &str, args: Json) -> String {
    Json::obj(vec![("id", Json::Num(1.0)), ("op", Json::str(op)), ("args", args)]).to_string()
}

pub fn start(handle: &LoopHandle<'static, App>, socket: PathBuf) -> Result<Client, String> {
    let (tx, rx) = channel::channel::<Msg>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(m) = ev {
                app.habit_msg(m);
            }
        })
        .map_err(|e| e.to_string())?;
    handle
        .insert_source(Timer::from_duration(Duration::from_secs(2)), |_, _, app: &mut App| {
            app.habit_refresh("timer");
            TimeoutAction::ToDuration(REFRESH)
        })
        .map_err(|e| e.to_string())?;
    let (jtx, jrx) = mpsc::sync_channel::<Job>(QUEUE);
    std::thread::Builder::new().name("bitscomp-habits".into()).spawn(move || work(&socket, &jrx, &tx)).map_err(|e| e.to_string())?;
    Ok(Client { tx: jtx, dropped: std::cell::Cell::new(0) })
}

impl Client {
    /// Queues `job`; false when the queue is full or the worker is gone (the job is dropped).
    pub fn send(&self, job: Job) -> bool {
        match self.tx.try_send(job) {
            Ok(()) => true,
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.dropped.set(self.dropped.get() + 1);
                false
            }
        }
    }
}

fn work(socket: &PathBuf, jobs: &mpsc::Receiver<Job>, out: &Sender<Msg>) {
    let mut conn: Option<(UnixStream, BufReader<UnixStream>)> = None;
    let mut said = false;
    while let Ok(job) = jobs.recv() {
        let (tag, text) = match job {
            Job::Event(l) => (None, l),
            Job::Ask(t, l) => (Some(t), l),
        };
        let answer = ask(socket, &mut conn, &text);
        match (&answer, &tag) {
            (Err(e), _) if !said => {
                eprintln!("bitscomp: habits: ctxd not reachable ({e}); events are dropped until it is");
                said = true;
            }
            (Ok(_), _) => said = false,
            _ => {}
        }
        let Some(tag) = tag else { continue };
        let m = match answer {
            Ok(j) => Msg::Reply(tag, j),
            Err(e) => Msg::Failed(tag, e),
        };
        if out.send(m).is_err() {
            return;
        }
    }
}

/// One line out, one line back, on the kept connection (made anew once when it broke).
fn ask(socket: &PathBuf, conn: &mut Option<(UnixStream, BufReader<UnixStream>)>, text: &str) -> Result<Json, String> {
    for _ in 0..2 {
        if conn.is_none() {
            let s = UnixStream::connect(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
            s.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
            s.set_write_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
            let r = s.try_clone().map_err(|e| e.to_string())?;
            *conn = Some((s, BufReader::new(r)));
        }
        let Some((w, r)) = conn.as_mut() else { continue };
        let mut got = String::new();
        let ok = writeln!(w, "{text}").is_ok() && r.by_ref().take(LINE_MAX).read_line(&mut got).is_ok_and(|n| n > 0);
        if ok {
            return Json::parse(got.trim()).map_err(|e| format!("ctxd answered no JSON: {e}"));
        }
        *conn = None;
    }
    Err("ctxd closed the connection".into())
}
