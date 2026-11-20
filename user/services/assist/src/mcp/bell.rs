//! `assist --mcp --bell PATH`: this server runs under a helper program of a running assistant, and what the
//! chat would ask Georg is asked there, through that assistant's bell, on its screen.
//!
//! A yes from the bell lets a changing tool run, so the bell must be that assistant's and nobody else's. The
//! path proves nothing (whoever starts this program names it). The kernel says who listens: it must be this
//! user's, run this very program (`assist`), and be an ancestor of this process: the assistant that started
//! the helper that started this server. A program that listens somewhere and says yes to itself is none of
//! the three.

use super::gate::Owner;
use crate::provider::program::ask::{Ask, Kind, Reply, Why};
use crate::provider::program::bell;
use settingsd::peer::Peer;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

pub struct Bell {
    path: PathBuf,
}

/// Two names of one program file (`same`), given by the caller so a test can answer for files it has not.
pub type Same = dyn Fn(&Path, &Path) -> bool;

/// The same file on disk, whatever the two paths are called.
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(a), Ok(b)) if (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// Is the listener `peer` the assistant this process descends from? Err: why not (for the log).
pub fn judge(peer: &Peer, own_uid: u32, own_exe: Option<&Path>, own_pid: u32, parent: &bell::Parent, same: &Same) -> Result<(), String> {
    let Some(uid) = peer.uid else { return Err("the kernel names nobody who listens".into()) };
    if uid != own_uid {
        return Err(format!("pid {} listens as uid {uid}", peer.pid));
    }
    let (Some(exe), Some(own)) = (peer.exe.as_deref(), own_exe) else {
        return Err(format!("pid {}: the program that listens could not be told", peer.pid));
    };
    if !same(Path::new(exe), own) {
        return Err(format!("pid {} runs {exe}, not the assistant", peer.pid));
    }
    if peer.pid == own_pid || !bell::in_family(own_pid, peer.pid, parent) {
        return Err(format!("pid {} did not start this server", peer.pid));
    }
    Ok(())
}

impl Bell {
    pub fn at(path: &str) -> Bell {
        Bell { path: path.into() }
    }

    fn check(s: &UnixStream) -> Result<(), String> {
        let uid = usys::geteuid().ok().and_then(|u| u32::try_from(u).ok()).ok_or("no uid")?;
        let exe = std::env::current_exe().ok();
        judge(&settingsd::peer::of(s), uid, exe.as_deref(), std::process::id(), &bell::parent, &same_file)
    }
}

impl Owner for Bell {
    fn ask(&self, tool: &str, question: &str, must_ask: bool) -> Reply {
        let ask = Ask { id: String::new(), tool: tool.to_string(), kind: Kind::Other, text: question.to_string(), path: None, must_ask, words: true };
        bell::ring_at(&self.path, &ask, &Bell::check).unwrap_or_else(|why| {
            eprintln!("assist --mcp: the bell {} is not usable: {why}", self.path.display());
            Reply::No(Why::CannotAsk)
        })
    }
}

/// `assist --ask --bell PATH`: questions as lines on stdin, their answers as lines on stdout. For a helper
/// that is a script. The bell hears only the helper it was made for and what that one started.
pub fn relay(path: &str) -> i32 {
    use std::io::{BufRead, Write};
    let (stdin, mut out) = (std::io::stdin().lock(), std::io::stdout().lock());
    for line in stdin.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (id, reply) = match Ask::parse(&line) {
            Some(ask) => (ask.id.clone(), bell::ring_at(Path::new(path), &ask, &|_| Ok(())).unwrap_or(Reply::No(Why::Stopped))),
            None => (String::new(), Reply::No(Why::Stopped)),
        };
        if writeln!(out, "{}", reply.line(&id)).and_then(|_| out.flush()).is_err() {
            return 1;
        }
    }
    0
}

/// `assist --call --tool-socket PATH` (contract version 4): request lines on stdin (`{"id","name","input"}` or
/// `{"id","list":true}`), their answer lines on stdout. For a helper that is a script. The socket is used only
/// when the kernel shows the assistant this process descends from listening there: a result from anybody else
/// would be words the model takes for the tool's.
pub fn call_relay(path: &str) -> i32 {
    use std::io::{BufRead, BufReader, Write};
    let stream = UnixStream::connect(path).map_err(|e| format!("{path}: {e}")).and_then(|s| Bell::check(&s).map(|_| s));
    let s = match stream {
        Ok(s) => s,
        Err(why) => {
            eprintln!("assist --call: the tool socket is not usable: {why}");
            return 1;
        }
    };
    let (Ok(mut to), mut from) = (s.try_clone(), BufReader::new(s)) else { return 1 };
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let mut answer = String::new();
        if writeln!(to, "{}", line.trim()).is_err() || from.read_line(&mut answer).map_or(true, |n| n == 0) {
            eprintln!("assist --call: the tool socket closed");
            return 1;
        }
        if write!(out, "{answer}").and_then(|_| out.flush()).is_err() {
            return 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use settingsd::peer::Source;

    #[test]
    fn only_the_assistant_this_server_descends_from_is_the_owner() {
        // 10 (assist) -> 20 (the helper) -> 30 (its model's program) -> 40 (this server).
        let parent: Box<bell::Parent> = Box::new(|pid| match pid {
            40 => Some(30),
            30 => Some(20),
            20 => Some(10),
            10 | 77 => Some(1),
            _ => None,
        });
        let own = Path::new("/bin/assist");
        let peer = |pid: u32, uid: u32, exe: Option<&str>| Peer { pid, uid: Some(uid), exe: exe.map(str::to_string), by: Source::Kernel };
        let same: &Same = &|a, b| a == b;
        assert!(judge(&peer(10, 1000, Some("/bin/assist")), 1000, Some(own), 40, &*parent, same).is_ok());
        let refused = [
            (peer(20, 1000, Some("/home/user/helper")), "the helper itself listens"),
            (peer(30, 1000, Some("/bin/sh")), "a shell the model started"),
            (peer(77, 1000, Some("/bin/assist")), "an assistant that is no ancestor"),
            (peer(40, 1000, Some("/bin/assist")), "this very process"),
            (peer(10, 0, Some("/bin/assist")), "another user's assistant"),
            (peer(10, 1000, None), "a program that could not be told"),
            (Peer { pid: 0, uid: None, exe: None, by: Source::Given }, "nobody"),
        ];
        for (p, what) in refused {
            assert!(judge(&p, 1000, Some(own), 40, &*parent, same).is_err(), "{what}");
        }
        assert!(judge(&peer(10, 1000, Some("/bin/assist")), 1000, None, 40, &*parent, same).is_err(), "this program does not know its own file");
        assert!(same_file(Path::new("/proc/self/exe"), &std::env::current_exe().unwrap()) && !same_file(Path::new("/proc/self/exe"), Path::new("/")));
    }
}
