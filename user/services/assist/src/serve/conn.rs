//! One client: its lines in, the answers out. The gate is asked for every line, when the line's first byte
//! arrives and before anything of it is kept or looked at: who is not the session's compositor gets
//! `not_allowed` for whatever was sent, and the connection stays (the compositor may connect a moment before
//! the kernel names it as the screen's holder).

use super::events::{Level, q};
use super::lines::{Line, Lines};
use super::out::{Out, WRITE_WAIT};
use super::{Ctx, ops, proto};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often a silent connection is looked at (is the writer's side dead, has a stranger idled too long).
const POLL: Duration = Duration::from_secs(1);
/// A connection that was never served is closed when it has sent no whole line for this long. Bytes without
/// a line end count as silence: dripping them holds no thread here.
pub const STRANGER_IDLE: Duration = Duration::from_secs(30);
/// What is kept of a line from somebody who is not served: nothing of it is read as a request.
const STRANGER_LINE: usize = 1024;

pub fn client(ctx: &Arc<Ctx>, s: UnixStream, n: u64) {
    let ev = &ctx.parts.events;
    let peer = (ctx.parts.gate.seen)(&s);
    let Ok(w) = s.try_clone() else { return };
    if s.set_read_timeout(Some(POLL)).is_err() || w.set_write_timeout(Some(WRITE_WAIT)).is_err() {
        return;
    }
    let out = Arc::new(Out::new(w));
    let mut lines = Lines::new(s);
    let mut verdict: Result<String, String> = Err(String::new());
    // Was the last line served? Decides which change of the gate's mind is worth an event.
    let mut served: Option<bool> = None;
    let mut ever = false;
    let mut quiet = Instant::now();
    loop {
        let line = lines.next(&mut || {
            verdict = ctx.parts.gate.admit(&peer);
            if verdict.is_ok() { (ctx.parts.budget)() } else { STRANGER_LINE }
        });
        let waiting = match &line {
            Ok(Line::More) => true,
            Err(e) => matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut),
            Ok(_) => false,
        };
        if waiting {
            if out.dead() || (!ever && quiet.elapsed() >= ctx.parts.stranger_idle) {
                break;
            }
            continue;
        }
        let line = match line {
            Ok(Line::End) | Err(_) => break,
            Ok(l) => l,
        };
        quiet = Instant::now();
        let reply = match (&verdict, line) {
            (Err(why), _) => {
                // Once per connection and change of mind, not per line: a stranger cannot fill the journal.
                if served != Some(false) {
                    ev.emit(Level::Warn, "refused", &format!("client={n} {} why={}", peer.show(), q(why)));
                }
                served = Some(false);
                Some(proto::not_allowed())
            }
            (Ok(who), line) => {
                if served != Some(true) {
                    ev.emit(Level::Info, "client", &format!("client={n} who={}", q(who)));
                }
                (served, ever) = (Some(true), true);
                match line {
                    Line::Text(t) if t.trim().is_empty() => None,
                    Line::Text(t) => ops::request(ctx, n, &out, t.trim()),
                    _ => Some(proto::err("bad_request", "Die Zeile ist länger, als dieser Rechner gerade im Speicher halten kann.")),
                }
            }
        };
        if let Some(r) = reply
            && !out.line(&r)
        {
            break;
        }
    }
    for (chat, id) in ctx.state.left(n) {
        ev.emit(Level::Warn, "client.gone", &format!("client={n} chat={chat} id={id}"));
    }
}
