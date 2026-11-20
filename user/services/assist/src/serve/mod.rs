//! `assist serve` (CL-3s, EINS K1): the one assistant as a service for both views of the desktop, the sidebar and
//! the popup (Win+F1). One unix socket (`bitsos-assist.sock` in the user's runtime directory, 0600), one JSON
//! object per line each way; protocol v2 stands in plan/spec/assistent-eins/PLAN.md K1 and eins-a.md.
//!
//! Who does what, so that nothing waits on anything slow (B4):
//! - one thread per client ([`conn`]) reads its lines, asks the gate about the caller for every one ([`peer`])
//!   and answers every request but `ask` at once ([`ops`]);
//! - one thread per question ([`worker`]), at most three at once and one per chat, takes the chat's
//!   conversation and runs the question through the engine the console chat uses; its screen is [`stream`],
//!   which writes the events to the asker, and its desktop tools go to the asker too ([`desk`]);
//! - they share [`state::State`], locked for a few instructions at a time and never across a wait, and the
//!   chats' files ([`chats`]).
//!
//! `answer` with yes lets a tool run, so only the session's compositor is served: everybody else gets
//! `not_allowed` whatever they send. This mode never says yes to a tool by itself.
//!
//! The other direction is the sidebar's to judge: the socket's folder is writable by every program of the
//! user, so the path proves nothing about who listens. The sidebar asks the kernel who does and takes only
//! `/bin/assist`; [`peer::may_listen`] sees to it that this program listens on BitsOS only with the system's
//! own settings.

pub mod bind;
pub mod chats;
pub mod conn;
pub mod desk;
pub mod events;
pub mod fake;
pub mod history;
pub mod lines;
pub mod ops;
pub mod out;
pub mod peer;
pub mod proto;
pub mod state;
pub mod status;
pub mod stream;
pub mod worker;

use crate::settings::Settings;
use crate::term::sys;
use crate::tools::ToolSource;
use crate::tools::both::Tools;
use crate::tools::desk::Desk;
use crate::tools::local::Place;
use events::{Events, Level};
use std::ffi::OsString;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const SOCKET_NAME: &str = "bitsos-assist.sock";
/// How long a tool question waits for Georg; without an answer the call does not run. Twice the installer's
/// dialog: this question sits in the sidebar, not above everything.
pub const QUESTION_WAIT: Duration = Duration::from_secs(120);
/// How long a desktop call waits for the view's result (a step of `handeln` waits for Georg's yes first).
pub const DESK_WAIT: Duration = Duration::from_secs(30);

/// The socket of user `uid`: in `runtime` (`$XDG_RUNTIME_DIR`) when that names a place, else in
/// /run/user/<uid>. The sidebar looks by the same rule (bitscomp's `rundir.rs`, and it hands its choice to
/// the helper it starts as that variable), so the two always mean one file.
pub fn default_socket(runtime: Option<OsString>, uid: u32) -> PathBuf {
    let dir = runtime.filter(|d| !d.is_empty()).map_or_else(|| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from);
    dir.join(SOCKET_NAME)
}

/// Where a question sits: its chat, and the view that runs its desktop tools.
pub struct Seat<'a> {
    pub chat: &'a str,
    pub desk: Option<&'a dyn Desk>,
}

/// Makes the tools of a question that starts now (the worker calls it on its own thread).
pub type MakeTools = dyn for<'a> Fn(&'a dyn Settings, Seat<'a>) -> Box<dyn ToolSource + 'a> + Send + Sync;

/// What a service is made of; tests bring their own of each.
pub struct Parts {
    pub settings: Arc<dyn Settings + Send + Sync>,
    pub tools: Box<MakeTools>,
    /// The most bytes one line, answer or tool result may take, asked when each starts (`limits::budget`).
    pub budget: Box<dyn Fn() -> usize + Send + Sync>,
    pub gate: peer::Gate,
    pub events: Events,
    pub question_wait: Duration,
    /// How long a desktop call waits for the view ([`DESK_WAIT`]).
    pub desk_wait: Duration,
    /// How long a connection that was never served may go without a whole line (`conn::STRANGER_IDLE`).
    pub stranger_idle: Duration,
    /// The chats' folder ([`chats::default_dir`]).
    pub chats: PathBuf,
}

pub struct Ctx {
    pub parts: Parts,
    pub state: state::State,
    pub store: chats::Store,
    /// Chat files that could not be read at the start.
    pub skipped: usize,
    clients: AtomicU64,
}

/// The service without its socket: the chats read from their folder, the shared state.
pub fn start(parts: Parts) -> std::io::Result<Arc<Ctx>> {
    let store = chats::Store::at(parts.chats.clone());
    let state = state::State::default();
    let (loaded, skipped) = store.load_all();
    for s in &skipped {
        parts.events.emit(Level::Warn, "chat.skipped", &format!("file={} reason={}", events::q(&s.file), events::q(&s.why)));
    }
    // The newest sidebar chat is what a view of before K1 means.
    let newest = loaded.iter().filter(|l| l.meta.origin == crate::prompt::Origin::Sidebar).max_by_key(|l| l.meta.updated).map(|l| l.meta.id.clone());
    parts.events.emit(Level::Info, "chats", &format!("loaded={} skipped={}", loaded.len(), skipped.len()));
    for l in loaded {
        state.add(l.meta);
    }
    if let Some(n) = newest {
        state.set_default(&n);
    }
    Ok(Arc::new(Ctx { parts, state, store, skipped: skipped.len(), clients: AtomicU64::new(0) }))
}

/// Serves `l` until the process ends: a thread per client (R1: as many as come).
pub fn serve(ctx: &Arc<Ctx>, l: &UnixListener) {
    for s in l.incoming() {
        match s {
            Ok(s) => {
                let n = ctx.clients.fetch_add(1, Ordering::Relaxed) + 1;
                let c = ctx.clone();
                if let Err(e) = std::thread::Builder::new().name("assist-client".into()).spawn(move || conn::client(&c, s, n)) {
                    ctx.parts.events.emit(Level::Error, "client.thread", &format!("reason={}", events::q(&e.to_string())));
                }
            }
            Err(e) => {
                ctx.parts.events.emit(Level::Error, "accept.failed", &format!("reason={}", events::q(&e.to_string())));
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// `assist serve [--listen PATH]`. Exit code 1: this user's assistant answers on the socket already (the
/// sidebar's keeper then waits for that one instead of starting another; a listener that is not the
/// assistant never causes it, its socket file is replaced); 2: the socket could not be made, or the world was
/// named from outside on BitsOS.
pub fn main(settings: Arc<dyn Settings + Send + Sync>, place: Place, listen: Option<&str>) -> i32 {
    let uid = match usys::geteuid().ok().and_then(|u| u32::try_from(u).ok()) {
        Some(u) => u,
        None => {
            eprintln!("assist serve: Der eigene Benutzer ließ sich nicht feststellen.");
            return 2;
        }
    };
    if !peer::may_listen(&place) {
        eprintln!("assist serve: Auf BitsOS arbeitet dieser Dienst nur mit den Einstellungen, Zertifikaten und Werkzeugen des Systems (--socket, SETTINGS_SOCKET, SSL_CERT_FILE und TOOLS_SOCKET gelten hier nicht). Beendet.");
        return 2;
    }
    // Before the first word to the tools socket: it admits this program only with its memory closed.
    if place == Place::Door {
        sys::close_memory();
    }
    let path = listen.map_or_else(|| default_socket(std::env::var_os("XDG_RUNTIME_DIR"), uid), PathBuf::from);
    let (l, replaced) = match bind::bind(&path, uid) {
        Ok(b) => b,
        Err(bind::Fail::Taken) => {
            eprintln!("assist serve: {}: dort antwortet schon ein Assistent. Dieser beendet sich.", path.display());
            return 1;
        }
        Err(bind::Fail::Other(why)) => {
            eprintln!("assist serve: {why}");
            return 2;
        }
    };
    let events = Events::system();
    events.emit(Level::Info, "start", &format!("socket={} uid={uid}", events::q(&path.to_string_lossy())));
    if let Some(p) = replaced {
        events.emit(Level::Warn, "replaced", &format!("socket={} listener: {}", events::q(&path.to_string_lossy()), p.show()));
    }
    // A panic's own words can quote the text it tripped over (a provider's answer): the journal gets the
    // place alone.
    let ev = events.clone();
    std::panic::set_hook(Box::new(move |info| {
        let at = info.location().map_or_else(|| "?".to_string(), |l| format!("{}:{}", l.file(), l.line()));
        ev.emit(Level::Error, "panic", &format!("at={}", events::q(&at)));
    }));
    let parts = Parts {
        settings,
        tools: Box::new(move |s, seat| Box::new(Tools::start(s, place.clone(), crate::limits::budget()).with_native(seat.chat).with_desk(seat.desk))),
        budget: Box::new(crate::limits::budget),
        gate: peer::Gate::system(uid),
        events,
        question_wait: QUESTION_WAIT,
        desk_wait: DESK_WAIT,
        stranger_idle: conn::STRANGER_IDLE,
        chats: chats::default_dir(),
    };
    match start(parts) {
        Ok(ctx) => serve(&ctx, &l),
        Err(e) => eprintln!("assist serve: Der Dienst ließ sich nicht starten ({e})."),
    }
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same table as bitscomp's `assist_client` test: the two programs must name one file.
    #[test]
    fn the_socket_is_in_the_runtime_directory_else_under_run_user() {
        assert_eq!(default_socket(Some("/run/user/1000".into()), 1000), PathBuf::from("/run/user/1000/bitsos-assist.sock"));
        assert_eq!(default_socket(Some("/tmp/bitscomp-1000".into()), 1000), PathBuf::from("/tmp/bitscomp-1000/bitsos-assist.sock"));
        assert_eq!(default_socket(None, 1000), PathBuf::from("/run/user/1000/bitsos-assist.sock"));
        assert_eq!(default_socket(Some(OsString::new()), 7), PathBuf::from("/run/user/7/bitsos-assist.sock"));
    }
}
