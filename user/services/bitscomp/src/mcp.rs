//! MCP tools of bitscomp (WY-4), registered over MC-6's service socket (user/lib/mcpreg, plan/notes/mc-MC-6.md):
//! `windows` (read: every window with id, title, app_id, pid, hung, workspace, tile) and `window_kill` (change:
//! ends a window's program like "Programm beenden"). mcpreg's calls block, so they run on a thread of their own;
//! each call is handed to the main loop through a calloop channel and answered from there in one turn. The thread
//! waits for that answer at most `ANSWER_WAIT` (B4: neither side ever waits on the other without a bound). A call
//! the thread has given up on is never carried out later: `Request::state` decides once who has it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use mcpreg::{Event, Json, Reply, Safety, Service, Tool};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::channel::{self, Sender};

use crate::layout::Place;
use crate::state::App;

pub const SERVICE: &str = "bitscomp";
const ANSWER_WAIT: Duration = Duration::from_secs(2);
const BACKOFF_MIN: Duration = Duration::from_millis(200);
const BACKOFF_MAX: Duration = Duration::from_secs(5);

pub enum Ask {
    Windows,
    Kill(u64),
}

pub struct Request {
    ask: Ask,
    answer: SyncSender<Reply>,
    state: Arc<AtomicU8>,
}

const WAITING: u8 = 0;
/// The main loop took the call: it is answered within this turn.
const TAKEN: u8 = 1;
/// The thread answered "did not answer in time": the main loop must not carry it out any more.
const ABANDONED: u8 = 2;

/// The main loop's claim on a request; false when the thread has already given up on it.
fn take(state: &AtomicU8) -> bool {
    state.compare_exchange(WAITING, TAKEN, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// The thread's claim after its wait ran out; false when the main loop took the call first (its answer follows).
fn abandon(state: &AtomicU8) -> bool {
    state.compare_exchange(WAITING, ABANDONED, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

fn tools() -> Vec<Tool> {
    let none = r#"{"type":"object","properties":{},"additionalProperties":false}"#;
    let id = r#"{"type":"object","properties":{"id":{"type":"integer","minimum":1,"description":"window id from the tool windows"}},"required":["id"],"additionalProperties":false}"#;
    [
        Tool::new(
            "windows",
            "bitscomp's windows as a JSON list: id, title, app_id, pid (from the client's socket), hung (true when the \
             program has not answered a ping for 2 s and its window shows 'antwortet nicht'), workspace, tile (null when \
             floating), page, floating, visible, focused.",
            none,
            Safety::Read,
        ),
        Tool::new(
            "window_kill",
            "Ends the program of window `id` (from windows), as the window's 'Programm beenden' does: SIGTERM, SIGKILL \
             2 s later if it is still there, then its windows go. Unsaved work in that program is lost.",
            id,
            Safety::Change,
        ),
    ]
    .into_iter()
    .filter_map(Result::ok)
    .collect()
}

/// Starts the service thread; `socket` is MC-6's service socket.
pub fn start(handle: &LoopHandle<'static, App>, socket: String) -> Result<(), String> {
    let (tx, rx) = channel::channel::<Request>();
    handle
        .insert_source(rx, |ev, _, app| {
            if let channel::Event::Msg(req) = ev {
                if !take(&req.state) {
                    eprintln!("bitscomp: MCP call dropped: its caller stopped waiting before the main loop came to it");
                    return;
                }
                // try_send into a channel of one that only this answer uses: never blocks.
                let _ = req.answer.try_send(app.mcp_answer(req.ask));
            }
        })
        .map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("bitscomp-mcp".into())
        .spawn(move || run(&socket, &tx))
        .map(|_| ())
        .map_err(|e| format!("MCP thread: {e}"))
}

fn run(socket: &str, tx: &Sender<Request>) {
    let mut backoff = BACKOFF_MIN;
    let mut last = String::new();
    let mut note = |what: String| {
        if what != last {
            eprintln!("bitscomp: MCP {what}");
            last = what;
        }
    };
    loop {
        match session(socket, tx, &mut note) {
            // A session that had registered starts the next try quickly again.
            Ok(true) => backoff = BACKOFF_MIN,
            Ok(false) => {}
            Err(Stop) => return,
        }
        std::thread::sleep(backoff);
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

struct Stop;

/// One connection to the registry: whether the tools were registered; `Err(Stop)` when the main loop is gone.
fn session(socket: &str, tx: &Sender<Request>, note: &mut impl FnMut(String)) -> Result<bool, Stop> {
    let mut svc = match Service::connect_at(socket, SERVICE) {
        Ok(s) => s,
        Err(e) => {
            note(format!("tools not registered yet: {socket}: {e} (retrying)"));
            return Ok(false);
        }
    };
    match svc.register(&tools()) {
        Ok(names) => note(format!("tools registered: {}", names.join(", "))),
        Err(e) => {
            note(format!("tools refused: {e} (retrying)"));
            return Ok(false);
        }
    }
    let replier = svc.replier();
    loop {
        let call = match svc.next() {
            Ok(Some(Event::Call(c))) => c,
            Ok(Some(Event::Cancelled { .. })) => continue,
            Ok(None) => {
                note("registry closed the connection (retrying)".into());
                return Ok(true);
            }
            Err(e) => {
                note(format!("connection lost: {e} (retrying)"));
                return Ok(true);
            }
        };
        let reply = match ask_of(&call.name, &call.arguments) {
            Err(r) => r,
            Ok(ask) => {
                let (atx, arx) = sync_channel(1);
                let state = Arc::new(AtomicU8::new(WAITING));
                if tx.send(Request { ask, answer: atx, state: state.clone() }).is_err() {
                    return Err(Stop);
                }
                let wait = if call.timeout_ms > 0 { ANSWER_WAIT.min(Duration::from_millis(call.timeout_ms)) } else { ANSWER_WAIT };
                match arx.recv_timeout(wait) {
                    Ok(r) => r,
                    Err(_) if abandon(&state) => Reply::error("bitscomp did not answer in time (nothing was done)"),
                    // Taken just now: the main loop answers within the same turn.
                    Err(_) => arx.recv_timeout(ANSWER_WAIT).unwrap_or_else(|_| Reply::error("bitscomp did not answer in time")),
                }
            }
        };
        if let Err(e) = replier.reply(call.id, reply) {
            note(format!("reply failed: {e} (retrying)"));
            return Ok(true);
        }
    }
}

fn ask_of(name: &str, args: &Json) -> Result<Ask, Reply> {
    match name {
        "windows" => Ok(Ask::Windows),
        "window_kill" => match args.get("id").and_then(Json::as_u64) {
            Some(id) if id > 0 => Ok(Ask::Kill(id)),
            _ => Err(Reply::bad_argument("id must be a window id (a positive integer from windows)")),
        },
        other => Err(Reply::error(format!("bitscomp has no tool {other}"))),
    }
}

impl App {
    fn mcp_answer(&mut self, ask: Ask) -> Reply {
        match ask {
            Ask::Windows => {
                let mut list = String::new();
                // The shown workspace's windows, then those of the parked ones (not visible).
                let mut all: Vec<Json> = self.placements().iter().map(|p| self.window_json(p, self.desk.active)).collect();
                for (i, ws) in self.desk.parked.iter().enumerate().filter(|(i, _)| *i != self.desk.active) {
                    for mut p in ws.arrange(self.area(), self.area_full(), self.theme.gap) {
                        p.visible = false;
                        p.focused = false;
                        all.push(self.window_json(&p, i));
                    }
                }
                Json::Arr(all).write_to(&mut list);
                Reply::text(list)
            }
            Ask::Kill(id) => {
                let Some(w) = self.all_windows().into_iter().find(|w| crate::ctl::win_id(w) == id) else {
                    return Reply::bad_argument(format!("no window {id} (see the tool windows)"));
                };
                match self.end_window(&w, "MCP window_kill") {
                    Ok(t) => Reply::text(t),
                    Err(e) => Reply::error(e),
                }
            }
        }
    }

    fn window_json(&self, p: &crate::layout::Placement<smithay::desktop::Window>, ws: usize) -> Json {
        let (app_id, title) = crate::ctl::names(&p.win);
        let num = |v: f64| Json::Num(v);
        let (tile, page, floating) = match p.place {
            Place::Tiled { tile, index, .. } => (num(tile as f64), num(index as f64), false),
            Place::Floating { .. } => (Json::Null, num(0.0), true),
        };
        Json::obj(vec![
            ("id", num(crate::ctl::win_id(&p.win) as f64)),
            ("title", Json::str(&title)),
            ("app_id", Json::str(&app_id)),
            ("pid", num(self.pid_of(&p.win) as f64)),
            ("hung", Json::Bool(self.is_hung(&p.win))),
            // The workspace's index (0 = the first in the list), WY-5.
            ("workspace", num(ws as f64)),
            ("tile", tile),
            ("page", page),
            ("floating", Json::Bool(floating)),
            ("visible", Json::Bool(p.visible)),
            ("focused", Json::Bool(p.focused)),
            // WY-8: the client's own fullscreen (on a parked workspace: covers the screen once it is shown).
            ("fullscreen", Json::Bool(p.fullscreen)),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_valid_and_named_as_planned() {
        let t = tools();
        assert_eq!(t.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["windows", "window_kill"]);
        assert_eq!((t[0].safety, t[1].safety), (Safety::Read, Safety::Change));
    }

    #[test]
    fn kill_needs_a_positive_id() {
        let args = |s: &str| Json::parse(s).unwrap();
        assert!(matches!(ask_of("window_kill", &args(r#"{"id":3}"#)), Ok(Ask::Kill(3))));
        assert!(matches!(ask_of("window_kill", &args(r#"{"id":0}"#)), Err(Reply::BadArgument(_))));
        assert!(matches!(ask_of("window_kill", &args(r#"{}"#)), Err(Reply::BadArgument(_))));
        assert!(matches!(ask_of("windows", &args(r#"{}"#)), Ok(Ask::Windows)));
        assert!(matches!(ask_of("nope", &args(r#"{}"#)), Err(Reply::Error(_))));
        for bad in [r#"{"id":-1}"#, r#"{"id":"3"}"#, r#"{"id":1.5}"#, r#"{"id":18446744073709551616}"#, r#"{"id":null}"#, r#"{"id":[3]}"#] {
            assert!(matches!(ask_of("window_kill", &args(bad)), Err(Reply::BadArgument(_))), "{bad}");
        }
    }

    #[test]
    fn an_abandoned_call_is_never_carried_out() {
        let s = AtomicU8::new(WAITING);
        assert!(abandon(&s));
        assert!(!take(&s));
        let s = AtomicU8::new(WAITING);
        assert!(take(&s));
        assert!(!abandon(&s));
    }
}
